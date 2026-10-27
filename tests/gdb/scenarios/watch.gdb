# Write watchpoint (Z2) on a global: GDB stops after the writing instruction
# with old and new values. @reverse lines check reverse-continue: it stops
# before the write; one stepi redoes it and must land on the state the
# forward run printed after that write.
@cores m4 m35p m4be
break build_list
continue
delete
watch head
continue
@dump watch_1
continue
@dump watch_2
continue
@dump watch_3
info watchpoints
@reverse reverse-continue
@reverse stepi
@reverse @dump watch_3
@reverse reverse-continue
@reverse reverse-continue
@reverse stepi
@reverse @dump watch_2
