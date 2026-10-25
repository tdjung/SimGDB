# SimGDB helper commands for gdb. Load with:  source tools/simgdb.gdb
#
# `monitor goto` moves SimGDB to another point, but gdb does not know the
# target changed; these wrappers also drop gdb's cached registers and memory
# and print the new frame.

define simgdb-goto
  monitor goto $arg0
  maintenance flush register-cache
  maintenance flush dcache
  frame
end
document simgdb-goto
Move to instruction index N and show the frame there: simgdb-goto N
end

define simgdb-goto-time
  monitor goto time $arg0
  maintenance flush register-cache
  maintenance flush dcache
  frame
end
document simgdb-goto-time
Move to the first instruction at or after trace time T: simgdb-goto-time T
end

define simgdb-time
  monitor time
end
document simgdb-time
Show the current point: instruction index, trace time, pc.
end

define simgdb-info
  monitor info
end
document simgdb-info
Show what the trace records and what SimGDB cannot see.
end
