#!/usr/bin/env bash
# Show the LeBOS boot banner in colour.
#
# Double-clickable: the read at the end keeps the window open, which is the
# only reason a script like this ever needs a last line.
cd "$(dirname "$0")" || exit 1
clear
cat reference/banner.txt
echo
read -rp "press enter to close "
