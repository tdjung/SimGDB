# Stops inside SysTick and SVC handlers; backtrace through exception frames.
@cores m4 m35p m4be
break SysTick_Handler
continue
@dump exc_a
continue
continue
@dump exc_b
p ticks
p/x ring
delete
break SVC_Handler
continue
@dump exc_c
p svc_calls
