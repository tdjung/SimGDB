# Floating-point registers (hard-float build): D/S registers and FPSCR, float
# arguments and return values, callee-saved S16-S31 of callers (unwound from
# VPUSH by the call frame information), backtraces through extended (FP)
# exception frames.
@cores m4f
break fscale
continue
continue
continue
info registers s0 s1 s16 s17 s18 d0 d8 fpscr
p x
p k
bt
finish
p $s0
p fvalues
up
info registers s16 s17
p acc
p a
p b
p keep
delete
break fp_work
continue
continue
info registers s16 fpscr
p/x $fpscr
up
info registers s16 d8
p acc
p round
delete
break walk_list
continue
bt
info registers s16 s17 fpscr control
p fresult
p fpscr_seen
delete
break use_process_stack
continue
delete
break SysTick_Handler
continue
continue
info registers sp msp psp control
bt
info registers s0 s1 s16 s17
