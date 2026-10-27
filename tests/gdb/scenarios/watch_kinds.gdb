# Read (Z3) and access (Z4) watchpoints, and a write watchpoint on an array
# element written from an exception handler.
@cores m4 m35p m4be
break build_list
continue
delete
rwatch seed
continue
@dump rwatch_1
continue
@dump rwatch_2
delete
awatch svc_calls
continue
@dump awatch_1
continue
@dump awatch_2
continue
@dump awatch_3
delete
watch ring[3]
continue
@dump watch_ring
continue
@dump watch_ring_2
