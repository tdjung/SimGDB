# Breakpoint, next, step, finish, stepi and data inspection in thread mode.
@cores m4 m35p m4be
break build_list
continue
@dump steps_a
next
@dump steps_b
next
next
@dump steps_c
step
@dump steps_d
finish
@dump steps_e
stepi
stepi
stepi
@dump steps_f
p primes
p/x table
p seed
p banner
x/s banner
p pool[0]
p *head
delete
break walk_list
continue
finish
@dump steps_g
