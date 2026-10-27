# Thread mode runs on the process stack after use_process_stack(): SP follows
# PSP, and exception frames are pushed on PSP. Breakpoints avoid PCs where an
# interrupt preempts the instruction in this run (see README).
@cores m4 m35p m4be
break walk_list
ignore 1 3
continue
@dump psp_a
finish
@dump psp_b
delete
break SysTick_Handler
continue
@dump psp_c
delete
break SVC_Handler
continue
@dump psp_d
