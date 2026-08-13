"""
pykernel.py -- LeBOS, in Python, badly.

Gitignored. Not part of the build. Nothing here runs on a CPU, and none of it
is correct as an operating system. Its only job is to be a translation of the
ALGORITHMS in src/main.rs into a language where you can add a print statement
and hit run.

Read a section here, then read the matching section in main.rs. The shapes
should line up; the Rust just also has to survive the hardware.

    python3 pykernel.py

Every section is independent. Comment out the calls at the bottom to focus.

WHAT PYTHON HIDES FROM YOU, and why the Rust looks harder:

  * Python has no addresses. `x = [1,2,3]` lives somewhere, and you can never
    ask where. Every "address" below is an integer index into a fake memory
    array, which is exactly what an address IS -- Python just refuses to show
    you the real one.
  * Python has a garbage collector. It frees things for you. Half of main.rs
    exists because nothing frees anything unless the kernel does.
  * Python is never interrupted mid-statement. The real machine can stop
    between any two instructions, which is what every lock and every
    intr_off() in main.rs is defending against.
"""

# ===========================================================================
# 0. FAKE PHYSICAL MEMORY
#
# main.rs: there is no equivalent, because on the real machine this IS the RAM.
#
# One flat array of bytes. An "address" is an index into it. That is the whole
# idea, and it is the one Python normally hides.
# ===========================================================================

PAGE_SIZE = 4096
RAM_SIZE = 64 * PAGE_SIZE
RAM_BASE = 0x8000_0000  # the real board starts RAM here, not at 0

mem = bytearray(RAM_SIZE)


def read_u64(addr):
    """Read 8 bytes at a physical address. `mem[addr]` in Rust is a pointer
    dereference, which is why main.rs is full of `unsafe`."""
    off = addr - RAM_BASE
    return int.from_bytes(mem[off:off + 8], "little")


def write_u64(addr, value):
    off = addr - RAM_BASE
    mem[off:off + 8] = (value & 0xFFFF_FFFF_FFFF_FFFF).to_bytes(8, "little")


# ===========================================================================
# 1. THE PHYSICAL FRAME ALLOCATOR          main.rs: "Physical frame allocator"
#
# Hand out 4096-byte pages. The trick worth understanding: the list of free
# pages is stored INSIDE the free pages themselves. A free page is not being
# used for anything, so its first 8 bytes hold the address of the next free
# page. The allocator needs no memory of its own -- which is good, because at
# this point in the boot there is nowhere to get any.
# ===========================================================================

free_head = 0  # 0 means "the list is empty", exactly as in main.rs


def frame_init(start, end):
    """Thread every page from start to end onto the free list, backwards, so
    the lowest address ends up at the head."""
    global free_head
    addr = end - PAGE_SIZE
    while addr >= start:
        write_u64(addr, free_head)   # this page points at the old head...
        free_head = addr             # ...and becomes the new head
        addr -= PAGE_SIZE


def frame_alloc():
    """Pop the head of the list. Returns a PHYSICAL address, which is why
    main.rs has a whole rule about calling va() before touching it."""
    global free_head
    if free_head == 0:
        return None
    page = free_head
    free_head = read_u64(page)  # the note inside the page says who is next
    return page


def frame_free(page):
    global free_head
    write_u64(page, free_head)
    free_head = page


def demo_frames():
    print("--- 1. frame allocator ---")
    frame_init(RAM_BASE, RAM_BASE + RAM_SIZE)
    a, b, c = frame_alloc(), frame_alloc(), frame_alloc()
    print(f"  allocated {a:#x} {b:#x} {c:#x}")
    frame_free(b)
    d = frame_alloc()
    print(f"  freed {b:#x}, next alloc returned {d:#x}  (same page, reused)")

    # The double-free hazard CLAUDE.md warns about, demonstrated:
    frame_free(a)
    frame_free(a)          # <-- the bug
    x, y = frame_alloc(), frame_alloc()
    print(f"  after freeing {a:#x} twice: {x:#x} then {y:#x}  (the same page twice)")
    print("  ...and every other free page is now unreachable.")


# ===========================================================================
# 2. THE PAGE TABLE                                    main.rs: "Paging -- Sv39"
#
# Turn a virtual address into a physical one by looking it up in a tree.
#
# The real thing is three levels deep and the hardware walks it. Here it is a
# dict, which throws away the only interesting part -- so read the comment
# below about what a real page table entry actually is.
# ===========================================================================

# Permission bits. In the real thing these are bits 0-4 of a 64-bit number that
# ALSO contains the physical page address in bits 10-53. One integer holds both
# "where" and "who may touch it", which is why main.rs has pte() and pte_to_pa().
V, R, W, X, U = 1, 2, 4, 8, 16


class PageTable:
    def __init__(self):
        self.entries = {}   # virtual page number -> (physical page, flags)

    def map(self, vaddr, paddr, flags):
        self.entries[vaddr // PAGE_SIZE] = (paddr, flags)

    def translate(self, vaddr, want, user_mode):
        """What the MMU does on EVERY memory access, in hardware, always."""
        vpn, offset = divmod(vaddr, PAGE_SIZE)
        if vpn not in self.entries:
            return "PAGE FAULT: unmapped"
        paddr, flags = self.entries[vpn]
        if not flags & V:
            return "PAGE FAULT: invalid entry"
        if not flags & want:
            return "PAGE FAULT: permission denied (this is W^X doing its job)"
        # The U bit is not "is it mapped", it is "is it mapped for THEM".
        if user_mode and not flags & U:
            return "PAGE FAULT: kernel page, and you are not the kernel"
        if not user_mode and flags & U:
            return "PAGE FAULT: user page, and SUM is off (deliberately)"
        return paddr + offset


def demo_paging():
    print("--- 2. page table ---")
    pt = PageTable()
    pt.map(0x1000, 0x8800_0000, V | R | X | U)          # user code:  r-x
    pt.map(0x8000, 0x8800_1000, V | R | W | U)          # user data:  rw-
    pt.map(0xFFFF_8000, 0x8800_2000, V | R | W)         # kernel:     rw-, no U

    print("  user reads its code   ->", hex_or(pt.translate(0x1000, R, True)))
    print("  user WRITES its code  ->", hex_or(pt.translate(0x1000, W, True)))
    print("  user writes its data  ->", hex_or(pt.translate(0x8000, W, True)))
    print("  user reads the kernel ->", hex_or(pt.translate(0xFFFF_8000, R, True)))
    print("  kernel reads user mem ->", hex_or(pt.translate(0x8000, R, False)))
    print("  anyone touches 0x5000 ->", hex_or(pt.translate(0x5000, R, True)))


def hex_or(v):
    return hex(v) if isinstance(v, int) else v


# ===========================================================================
# 3. THE KERNEL HEAP                              main.rs: "Kernel heap -- 7a"
#
# Frames are 4096 bytes. A String is 37 bytes. Something has to cut pages into
# arbitrary sizes and stitch them back together. That is a heap.
#
# Free blocks form a list sorted by address. Allocated blocks are NOT in any
# list and carry no header at all -- Rust hands the size back on free, so only
# the GAPS need signs.
# ===========================================================================

class Heap:
    def __init__(self, start, size):
        # Each free block: [address, size]. Kept sorted by address, which is
        # what makes coalescing possible at all.
        self.free = [[start, size]]

    def alloc(self, size):
        """First fit: walk the list, take the first block big enough."""
        for block in self.free:
            if block[1] >= size:
                addr = block[0]
                leftover = block[1] - size
                if leftover >= 16:      # big enough to hold its own header
                    block[0] += size    # shrink the block, keep it in the list
                    block[1] = leftover
                else:
                    self.free.remove(block)   # too small to split; hand it all over
                return addr
        return None

    def free_block(self, addr, size, coalesce=True):
        self.free.append([addr, size])
        self.free.sort()
        if coalesce:
            self._coalesce()

    def _coalesce(self):
        """Merge blocks that touch. Without this the heap slowly turns into
        confetti: the same number of free bytes, in more and more pieces, until
        nothing large fits anywhere."""
        i = 0
        while i < len(self.free) - 1:
            here, nxt = self.free[i], self.free[i + 1]
            if here[0] + here[1] == nxt[0]:
                here[1] += nxt[1]
                self.free.pop(i + 1)
            else:
                i += 1

    def stats(self):
        return len(self.free), sum(b[1] for b in self.free)


def demo_heap():
    print("--- 3. heap ---")
    h = Heap(0x8100_0000, 4096)
    a = h.alloc(100)
    b = h.alloc(200)
    c = h.alloc(50)
    print(f"  allocated {a:#x} {b:#x} {c:#x}   free: {h.stats()}")
    h.free_block(b, 200)
    print(f"  freed the middle one         free: {h.stats()}")
    print(f"  next alloc(200) -> {h.alloc(200):#x}  (handed straight back)")

    # The planted bug from CLAUDE.md: turn coalescing off and watch the byte
    # count stay identical while the block count climbs. Same memory, more
    # confetti, and eventually a large request fails with plenty free.
    print("  without coalescing:")
    h2 = Heap(0x8200_0000, 4096)
    xs = [h2.alloc(64) for _ in range(8)]
    for x in xs:
        h2.free_block(x, 64, coalesce=False)
    print(f"    blocks={h2.stats()[0]:2}  bytes={h2.stats()[1]}   <- 8 pieces")
    h3 = Heap(0x8200_0000, 4096)
    xs = [h3.alloc(64) for _ in range(8)]
    for x in xs:
        h3.free_block(x, 64, coalesce=True)
    print(f"    blocks={h3.stats()[0]:2}  bytes={h3.stats()[1]}   <- 1 piece, same bytes")


# ===========================================================================
# 4. THE BUMP ALLOCATOR                         user/src/main.rs, milestone 17
#
# The one you specified. One pointer walks forward; free does nothing.
# ===========================================================================

CHUNK = 64 * 1024


class Bump:
    def __init__(self, break_addr):
        self.next = break_addr
        self.limit = break_addr
        self.brk = break_addr
        self.syscalls = 0

    def sbrk(self, n):
        """The kernel side. Returns the OLD break -- the start of the new land."""
        self.syscalls += 1
        old = self.brk
        self.brk += n
        return old

    def alloc(self, size, align):
        while True:
            start = align_up(self.next, align)
            if start + size <= self.limit:
                self.next = start + size
                return start
            want = CHUNK if size + align <= CHUNK else align_up(size + align, PAGE_SIZE)
            got = self.sbrk(want)
            if got != self.limit:
                self.next = got
            self.limit = got + want

    def free(self, addr):
        pass  # on purpose. thread_exit unmaps the whole address space at once.


def align_up(x, a):
    """Smallest multiple of `a` that is >= x. Adding a-1 pushes anything not
    already on a boundary past the next one; the mask chops it back down."""
    return (x + a - 1) & ~(a - 1)


def demo_bump():
    print("--- 4. bump allocator (yours) ---")
    b = Bump(0xC000)
    print(f"  alloc(10, 1)  -> {b.alloc(10, 1)}")
    print(f"  alloc(4,  8)  -> {b.alloc(4, 8)}   <- skipped forward to a multiple of 8")
    print(f"  alloc(2,  1)  -> {b.alloc(2, 1)}")
    print(f"  alloc(100,16) -> {b.alloc(100, 16)}  <- skipped forward to a multiple of 16")
    before = b.brk
    b.alloc(100 * 1024, 1)
    print(f"  alloc(102400) -> break grew by {b.brk - 0xC000}, in {b.syscalls} syscalls")


# ===========================================================================
# 5. THE SCHEDULER                             main.rs: "Threads -- milestone 8"
#
# Python cannot show you a context switch: `switch` saves 14 registers and its
# final `ret` jumps into a DIFFERENT function than the one that called it.
# There is no Python for that.
#
# What Python CAN show is the bookkeeping around it, which is where the bugs
# actually live: who is runnable, who is asleep on what, and who is dead but
# not yet buried.
# ===========================================================================

RUNNABLE, SLEEPING, ZOMBIE, FREE = "runnable", "sleeping", "zombie", "free"


class Thread:
    def __init__(self, name, parent=None):
        self.name = name
        self.state = RUNNABLE
        self.chan = None      # what it is waiting for, when sleeping
        self.exit_code = None
        self.parent = parent
        self.stack = bytearray(16 * 1024)   # the 16 KiB nobody can free from inside


threads = []
current = 0


def yield_now():
    """Round robin over RUNNABLE threads only. Starting at current+1 is what
    makes it fair; going all the way round to current itself is what lets a
    lone runnable thread keep running."""
    global current
    n = len(threads)
    if n == 0:
        return                       # the empty-table guard that cost an hour
    for k in range(1, n + 1):
        i = (current + k) % n
        if threads[i].state == RUNNABLE:
            current = i
            return
    # Nothing runnable. The real kernel executes `wfi` here and waits for an
    # interrupt to make someone runnable. Python has no interrupts, so:
    raise SystemExit("every thread is asleep and nothing can wake them")


def sleep(chan):
    """Interrupts must ALREADY be off, and have been off since the condition
    was tested. See the lost wakeup below."""
    threads[current].state = SLEEPING
    threads[current].chan = chan
    yield_now()


def wakeup(chan):
    for t in threads:
        if t.state == SLEEPING and t.chan == chan:
            t.state = RUNNABLE
            t.chan = None


def thread_exit(code):
    """Phase one. The address space goes; the kernel stack CANNOT, because this
    code is standing on it."""
    me = threads[current]
    for t in threads:                       # re-parent orphans to init, so a
        if t.parent is me:                  # reused slot cannot adopt them
            t.parent = threads[0]
    me.state = ZOMBIE
    me.exit_code = code
    if me.parent:
        wakeup(me.parent)                   # the channel IS the parent object
    yield_now()


def thread_wait():
    """Phase two, run by somebody else. NOW the stack can go."""
    me = threads[current]
    while True:
        for t in threads:
            if t.parent is me and t.state == ZOMBIE:
                code, name = t.exit_code, t.name
                t.state = FREE
                t.stack = None              # <- the 16 KiB, finally
                return name, code
        if not any(t.parent is me and t.state != FREE for t in threads):
            return None
        sleep(me)


def demo_scheduler():
    print("--- 5. scheduler, sleep, exit, reap ---")
    global threads, current
    threads, current = [], 0
    init = Thread("init")
    threads.append(init)
    threads.append(Thread("shell", parent=init))
    threads.append(Thread("child", parent=init))

    print("  ", [f"{t.name}:{t.state}" for t in threads])

    # The shell blocks waiting for a keystroke.
    current = 1
    threads[1].state = SLEEPING
    threads[1].chan = "console"
    print("   shell sleeps on 'console'")
    print("  ", [f"{t.name}:{t.state}" for t in threads])

    # The child dies. Nobody has collected it yet -> zombie.
    current = 2
    threads[2].state = ZOMBIE
    threads[2].exit_code = 0
    print("   child exits (nobody has reaped it yet)")
    print("  ", [f"{t.name}:{t.state}" for t in threads])

    # init reaps it.
    current = 0
    print("   init reaps ->", thread_wait())
    print("  ", [f"{t.name}:{t.state}" for t in threads])

    # A keystroke arrives.
    wakeup("console")
    print("   keystroke arrives, wakeup('console')")
    print("  ", [f"{t.name}:{t.state}" for t in threads])


def demo_lost_wakeup():
    """The bug sleep() exists to prevent. Read this one twice."""
    print("--- 5b. the lost wakeup ---")
    buffer = []
    asleep = False

    def interrupt():
        buffer.append(ord("a"))
        if asleep:
            print("      wakeup lands on a sleeping thread -> woken")
        else:
            print("      wakeup SHOUTS INTO AN EMPTY ROOM -- nobody is asleep yet")

    print("   WRONG (interrupts left on between the check and the sleep):")
    if not buffer:                 # (1) check: empty
        interrupt()                # (2) the interrupt lands HERE
        asleep = True              # (3) sleep -- forever, with data waiting
    print(f"      buffer={buffer} asleep={asleep}  <- data available, thread asleep")

    print("   RIGHT (interrupts off across all three):")
    print("      the interrupt cannot land at (2), so either the buffer was")
    print("      already non-empty and we never sleep, or we are genuinely")
    print("      asleep before anyone can shout.")


# ===========================================================================
# 6. THE OBJECT STORE                    main.rs: "The object store -- 12"
#
# The part that is not in any other OS. Python is actually a fair model here,
# because it is all data structures rather than hardware.
# ===========================================================================

def fnv1a(data):
    """The same hash main.rs uses. NOT cryptographic -- swap for SHA-256 before
    anything untrusted can write to the store."""
    h = 0xcbf29ce484222325
    for byte in data:
        h ^= byte
        h = (h * 0x100000001b3) & 0xFFFF_FFFF_FFFF_FFFF
    return h


blobs = {}    # hash(bytes)             -> bytes          stored once
store = {}    # hash(blob + attrs)      -> object         one per STATEMENT
claims = []   # (time, id, key, value)  append-only       how anything changes
clock = [0]


def store_create(content: bytes, attrs: dict):
    """Two hashes, and the split between them is the whole design.

    Hashing only the bytes lost data: two different documents that happened to
    contain identical content collapsed into one, and the second one's name was
    silently discarded. So the BYTES are addressed by their own hash and stored
    once, and the OBJECT -- which is a statement ABOUT those bytes -- is
    addressed by a hash of the metadata plus which blob it points at."""
    content_id = fnv1a(content)
    blobs.setdefault(content_id, content)

    material = str(content_id) + repr(sorted(attrs.items()))
    obj_id = fnv1a(material.encode())
    store.setdefault(obj_id, {"id": obj_id, "content": content_id, "attrs": attrs})
    return obj_id


def claim(obj_id, key, value):
    """Objects are content-addressed, so changing an attribute would change the
    id. Mutation is therefore an append: 'as of time T, X's K is V'. The current
    value is simply the latest claim. Nothing is overwritten, so WHEN something
    was hidden stays answerable."""
    clock[0] += 1
    claims.append((clock[0], obj_id, key, value))


def current_claim(obj_id, key):
    matching = [c for c in claims if c[1] == obj_id and c[2] == key]
    return max(matching)[3] if matching else None


def is_hidden(obj_id):
    return current_claim(obj_id, "hidden") == 1


def query(conds, hidden=False):
    """A linear scan, deliberately. Indexes are an optimisation; the semantics
    have to be right before the speed matters."""
    out = []
    for obj in store.values():
        if all(match(obj, c) for c in conds) and is_hidden(obj["id"]) == hidden:
            out.append(obj)
    return out


def match(obj, cond):
    key, op, want = cond
    have = obj["attrs"].get(key)
    if have is None:
        return False
    if op == "=":
        return have == want
    if op == "~":
        return isinstance(have, str) and want in have
    if op == ">":
        return isinstance(have, int) and have > want
    if op == "<":
        return isinstance(have, int) and have < want
    return False


def evict(obj_id):
    """SPACE. The bytes go, the record stays -- possible only because ids are
    content hashes, so the object remains a valid coordinate with nothing
    behind it. 'The file I was working on while that video was open' still
    answers after the video is gone. No filesystem can do this."""
    content = store[obj_id]["content"]
    still_used = any(o["content"] == content and o["id"] != obj_id for o in store.values())
    if not still_used:
        blobs.pop(content, None)
    claim(obj_id, "evicted", 1)


def forget(obj_id):
    """PRIVACY. The record goes too. The difference from evict is deliberate:
    eviction leaves a tombstone because you still want to reason about the
    thing; forgetting leaves nothing because you should not."""
    evict(obj_id)
    store.pop(obj_id, None)
    claim(obj_id, "forgotten", 1)


def demo_store():
    print("--- 6. the object store ---")
    a = store_create(b"import pygame", {"name": "brick breaker", "type": "python", "t": 101})
    store_create(b"remember the paddle", {"name": "notes", "type": "text", "t": 101})
    store_create(b"def solve(): pass", {"name": "solver", "type": "python", "t": 100})

    # Same bytes, different statements about them. The bug that forced the split.
    x = store_create(b"1040", {"name": "tax return"})
    y = store_create(b"1040", {"name": "shopping list"})
    print(f"   identical bytes, different names -> distinct objects: {x != y}")
    print(f"   {len(store)} objects but only {len(blobs)} blobs -- content stored once")

    print("   type=python           ->", [o["attrs"]["name"] for o in query([("type", "=", "python")])])
    print("   type=python AND t>100 ->", [o["attrs"]["name"] for o in
                                          query([("type", "=", "python"), ("t", ">", 100)])])
    print("   name~brick            ->", [o["attrs"]["name"] for o in query([("name", "~", "brick")])])

    claim(a, "hidden", 1)
    print("   after hiding it, type=python ->",
          [o["attrs"]["name"] for o in query([("type", "=", "python")])])
    print("   the 'cluttered' view          ->",
          [o["attrs"]["name"] for o in query([], hidden=True)])
    print("   ...which is not a folder, it is the same query with one flag flipped")

    evict(a)
    print(f"   evicted: bytes gone ({store[a]['content'] not in blobs}), "
          f"record still there ({a in store})")
    forget(a)
    print(f"   forgotten: record gone too ({a not in store})")
    print(f"   {len(claims)} claims recorded -- nothing overwritten, so WHEN is answerable")


# ===========================================================================

if __name__ == "__main__":
    demo_frames()
    print()
    demo_paging()
    print()
    demo_heap()
    print()
    demo_bump()
    print()
    demo_scheduler()
    print()
    demo_lost_wakeup()
    print()
    demo_store()
