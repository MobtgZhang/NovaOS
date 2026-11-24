.equ CSR_CRMD,        0x00
.equ CSR_PRMD,        0x01
.equ CSR_EUEN,        0x02
.equ CSR_ECFG,        0x04
.equ CSR_ESTAT,       0x05
.equ CSR_ERA,         0x06
.equ CSR_BADV,        0x07
.equ CSR_EENTRY,      0x0c
.equ CSR_TLBIDX,      0x10
.equ CSR_TLBEHI,      0x11
.equ CSR_TLBELO0,     0x12
.equ CSR_TLBELO1,     0x13
.equ CSR_STLBPS,      0x1e
.equ CSR_SAVE0,       0x30
.equ CSR_SAVE1,       0x31
.equ CSR_FATAL_SP,    0x32
.equ CSR_TCFG,        0x41
.equ CSR_TICLR,       0x44
.equ CSR_TLBRENTRY,   0x88
.equ CSR_TLBRBADV,    0x89
.equ CSR_TLBRERA,     0x8a
.equ CSR_TLBRSAVE,    0x8b
.equ CSR_TLBRELO0,    0x8c
.equ CSR_TLBRELO1,    0x8d
.equ CSR_TLBREHI,     0x8e
.equ CSR_TLBRPRMD,    0x8f
.equ CSR_DMW0,        0x180
.equ CSR_DMW1,        0x181
.equ CSR_DMW2,        0x182
.equ CSR_DMW3,        0x183

.equ CRMD_DIRECT,     0x08
.equ CRMD_PAGED,      0x10
.equ CRMD_BOOT_MASK,  0x1f
.equ EUEN_FPE,        {euen_fp}
.equ EUEN_LSX,        {euen_lsx}
.equ EUEN_LASX,       {euen_lasx}
.equ EUEN_EXTENSIONS, 0xf
.equ DMW_CACHED_PLV0, 0x11
.equ ESTAT_SYSCALL,   {syscall_exception}
.equ PLV3_MASKED,     0x03
.equ PRMD_MODE_MASK,  0x07
.equ PLV3_IRQ_ON,     0x07
.equ TIMER_IRQ,       (1 << 11)
.equ TIMER_ENABLE,    1
.equ TIMER_CLEAR,     1
.equ TIMER_TICKS,     100000
.equ PTE_VALID,       (1 << 0)
.equ PAGE_SHIFT,      {page_shift}
.equ PAGE_SIZE,       (1 << PAGE_SHIFT)
.equ USER_CODE,       {user_code}
.equ CONTEXT_PC,      {context_pc}
.equ CONTEXT_GPR,     {context_gpr}

.equ FRAME_CONTEXT,  0
.equ FRAME_SYSCALL,  8
.equ FRAME_USER_T1,  16
.equ FRAME_RA,       24
.equ FRAME_TP,       32
.equ FRAME_R21,      40
.equ FRAME_CRMD,     128
.equ FRAME_PRMD,     136
.equ FRAME_ERA,      144
.equ FRAME_DMW0,     152
.equ FRAME_DMW1,     160
.equ FRAME_DMW2,     168
.equ FRAME_DMW3,     176
.equ FRAME_STLBPS,   184
.equ FRAME_TLBIDX,   192
.equ FRAME_TLBEHI,   200
.equ FRAME_TLBELO0,  208
.equ FRAME_TLBELO1,  216
.equ FRAME_TCFG,     224
.equ FRAME_EUEN,     232
.equ FRAME_FCSR,     240
.equ FRAME_FPRS,     248
.equ FRAME_PAGE_ROOT, 312
.equ FRAME_REFILL_TEMP, 320
.equ FRAME_SIZE,     352

.macro save_csr csr, offset
    csrrd $t0, \csr
    st.d  $t0, $sp, \offset
.endm

.macro restore_csr csr, offset
    ld.d  $t0, $sp, \offset
    csrwr $t0, \csr
.endm

.macro user_address target, label, scratch
    la.local \target, \label
    andi     \target, \target, (PAGE_SIZE - 1)
    li.w     \scratch, USER_CODE
    or       \target, \target, \scratch
.endm

.section .text.loongarch64_user_probe, "ax", @progbits
.balign PAGE_SIZE
.globl loongarch64_user_start
loongarch64_user_start:
.Luser_code:
    xori    $a0, $a0, {cookie_salt}
    addi.d  $sp, $sp, -16
    st.d    $a0, $sp, 0
    or      $a0, $zero, $zero
    ld.d    $a0, $sp, 0
.globl loongarch64_user_cookie_syscall
loongarch64_user_cookie_syscall:
    syscall 0
    break   0
.globl loongarch64_user_abi_start
loongarch64_user_abi_start:
    or      $s0, $a0, $zero
    or      $s2, $a6, $zero
    or      $tp, $a0, $zero
    beqz    $s3, .Labi_no_fp_init
    movgr2fr.d $f31, $a0
    movgr2cf $fcc7, $a6
    slli.d  $t0, $a6, 8
    movgr2fcsr $fcsr0, $t0
    andi    $t0, $s3, EUEN_LSX
    beqz    $t0, .Labi_no_fp_init
    vinsgr2vr.d $vr31, $a0, 1
    andi    $t0, $s3, EUEN_LASX
    beqz    $t0, .Labi_no_fp_init
    nor     $t0, $a0, $zero
    xvinsgr2vr.d $xr31, $t0, 3
.Labi_no_fp_init:
    or      $t0, $a0, $zero
    or      $t1, $a0, $zero
    addi.d  $sp, $sp, -80
    st.d    $a0, $sp, 64
    li.w    $a7, {get_version}
.globl loongarch64_user_version_syscall
loongarch64_user_version_syscall:
    syscall 0
    li.d    $t2, {version_word}
    bne     $a0, $t2, .Labi_bad_exit
    bne     $s0, $t0, .Labi_bad_exit
    bne     $s0, $t1, .Labi_bad_exit
    bnez    $a6, .Labi_yield
    li.d    $a1, {timer_budget}
.Labi_wait:
    bnez    $a6, .Labi_resume
    addi.d  $a1, $a1, -1
    bnez    $a1, .Labi_wait
.Labi_wait_end:
    break   0
.Labi_yield:
    li.w    $a7, {yield_thread}
.globl loongarch64_user_yield_syscall
loongarch64_user_yield_syscall:
    syscall 0
    bnez    $a0, .Labi_bad_exit
.Labi_resume:
    or      $a0, $s1, $zero
    or      $a2, $sp, $zero
    bnez    $s2, .Labi_recv
    st.d    $s0, $sp, 0
    li.w    $a1, 0x31
    li.w    $a3, 8
    li.w    $a7, {send}
    b       loongarch64_user_ipc_syscall
.Labi_recv:
    or      $a4, $zero, $zero
    or      $a1, $zero, $zero
    li.w    $a3, {max_inline_payload}
    li.w    $a7, {recv}
.globl loongarch64_user_ipc_syscall
loongarch64_user_ipc_syscall:
    syscall 0
    beqz    $s2, .Labi_check_sender
    li.w    $t2, 0x31
    bne     $a0, $t2, .Labi_bad_exit
    ld.d    $t2, $sp, 0
    li.d    $t3, {cookie_salt}
    xor     $t3, $s0, $t3
    bne     $t2, $t3, .Labi_bad_exit
    b       .Labi_check_stack
.Labi_check_sender:
    bnez    $a0, .Labi_bad_exit
.Labi_check_stack:
    bne     $s0, $t0, .Labi_bad_exit
    bne     $s0, $t1, .Labi_bad_exit
    bne     $s0, $tp, .Labi_bad_exit
    beqz    $s3, .Labi_stack_cookie
    movfr2gr.d $t2, $f31
    bne     $s0, $t2, .Labi_bad_exit
    movcf2gr $t2, $fcc7
    bne     $s2, $t2, .Labi_bad_exit
    movfcsr2gr $t2, $fcsr0
    slli.d  $t3, $s2, 8
    bne     $t2, $t3, .Labi_bad_exit
    andi    $t2, $s3, EUEN_LSX
    beqz    $t2, .Labi_stack_cookie
    vpickve2gr.d $t2, $vr31, 1
    bne     $t2, $s0, .Labi_bad_exit
    andi    $t2, $s3, EUEN_LASX
    beqz    $t2, .Labi_stack_cookie
    xvpickve2gr.d $t2, $xr31, 3
    nor     $t3, $s0, $zero
    bne     $t2, $t3, .Labi_bad_exit
.Labi_stack_cookie:
    ld.d    $a0, $sp, 64
    bne     $a0, $s0, .Labi_bad_exit
    b       .Labi_exit
.Labi_bad_exit:
    or      $a0, $zero, $zero
.Labi_exit:
    addi.d  $sp, $sp, 80
    li.w    $a7, {thread_exit}
.globl loongarch64_user_exit_syscall
loongarch64_user_exit_syscall:
    syscall 0
    break   0
.balign PAGE_SIZE

.section .text.loongarch64_privilege, "ax", @progbits
.globl loongarch64_install_trap_vectors
.type loongarch64_install_trap_vectors, @function
loongarch64_install_trap_vectors:
    or       $t0, $sp, $zero
    csrwr    $t0, CSR_FATAL_SP
    la.local $t0, .Lexception_entry
    csrwr    $t0, CSR_EENTRY
    la.local $t0, .Lrefill_entry
    csrwr    $t0, CSR_TLBRENTRY
    or       $t0, $zero, $zero
    csrwr    $t0, CSR_ECFG
    or       $t0, $zero, $zero
    csrwr    $t0, CSR_SAVE0
    or       $t0, $zero, $zero
    csrwr    $t0, CSR_SAVE1
    ret
.size loongarch64_install_trap_vectors, . - loongarch64_install_trap_vectors

.globl loongarch64_enter_user
.type loongarch64_enter_user, @function
loongarch64_enter_user:
    csrrd    $t0, CSR_CRMD
    andi     $t0, $t0, CRMD_BOOT_MASK
    li.w     $t1, CRMD_DIRECT
    bne      $t0, $t1, .Lprobe_unavailable
    csrrd    $t0, CSR_EENTRY
    la.local $t1, .Lexception_entry
    bne      $t0, $t1, .Lprobe_unavailable
    li.w     $t0, {init_traps}
    bltu     $t0, $a1, .Lsave_context
    csrrd    $t0, CSR_ECFG
    bnez     $t0, .Lprobe_unavailable
    csrrd    $t0, CSR_TCFG
    andi     $t0, $t0, TIMER_ENABLE
    bnez     $t0, .Lprobe_unavailable
    csrrd    $t0, CSR_ESTAT
    andi     $t0, $t0, TIMER_IRQ
    bnez     $t0, .Lprobe_unavailable

.Lsave_context:
    addi.d   $sp, $sp, -FRAME_SIZE
    st.d     $a0, $sp, FRAME_CONTEXT
    st.d     $a1, $sp, FRAME_SYSCALL
    st.d     $a2, $sp, FRAME_PAGE_ROOT
    st.d     $ra, $sp, FRAME_RA
    st.d     $tp, $sp, FRAME_TP
    .irp reg, 21,22,23,24,25,26,27,28,29,30,31
        st.d $r\reg, $sp, (FRAME_R21 + 8 * (\reg - 21))
    .endr
    save_csr CSR_CRMD, FRAME_CRMD
    save_csr CSR_PRMD, FRAME_PRMD
    save_csr CSR_ERA, FRAME_ERA
    save_csr CSR_DMW0, FRAME_DMW0
    save_csr CSR_DMW1, FRAME_DMW1
    save_csr CSR_DMW2, FRAME_DMW2
    save_csr CSR_DMW3, FRAME_DMW3
    save_csr CSR_STLBPS, FRAME_STLBPS
    save_csr CSR_TLBIDX, FRAME_TLBIDX
    save_csr CSR_TLBEHI, FRAME_TLBEHI
    save_csr CSR_TLBELO0, FRAME_TLBELO0
    save_csr CSR_TLBELO1, FRAME_TLBELO1
    save_csr CSR_EUEN, FRAME_EUEN
    or       $t0, $a3, $zero
    li.w     $t1, EUEN_EXTENSIONS
    csrxchg  $t0, $t1, CSR_EUEN
    beqz     $a3, .Lsetup_mapping
    movfcsr2gr $t0, $fcsr0
    st.d     $t0, $sp, FRAME_FCSR
    .irp reg,24,25,26,27,28,29,30,31
        fst.d $f\reg, $sp, (FRAME_FPRS + 8 * (\reg - 24))
    .endr

.Lsetup_mapping:
    li.w     $t0, DMW_CACHED_PLV0
    csrwr    $t0, CSR_DMW0
    or       $t0, $zero, $zero
    csrwr    $t0, CSR_DMW1
    or       $t0, $zero, $zero
    csrwr    $t0, CSR_DMW2
    or       $t0, $zero, $zero
    csrwr    $t0, CSR_DMW3

    invtlb   0, $zero, $zero
    li.w     $t0, PAGE_SHIFT
    csrwr    $t0, CSR_STLBPS
.Lpublish_frame:
    or       $t0, $sp, $zero
    csrwr    $t0, CSR_FATAL_SP
    or       $t0, $sp, $zero
    csrwr    $t0, CSR_SAVE0
    li.w     $t0, {init_traps}
    bgeu     $t0, $a1, .Lprepare_timer
    li.w     $t0, PLV3_MASKED
    b        .Lenter_user

.Lprepare_timer:
    save_csr CSR_TCFG, FRAME_TCFG
    li.w     $t0, TIMER_IRQ
    csrwr    $t0, CSR_ECFG
    li.w     $t0, (TIMER_TICKS | TIMER_ENABLE)
    csrwr    $t0, CSR_TCFG
    li.w     $t0, PLV3_IRQ_ON

.Lenter_user:
    csrwr    $t0, CSR_PRMD
    beqz     $a3, .Lrestore_integer
    andi     $t0, $a3, EUEN_LASX
    bnez     $t0, .Lrestore_lasx
    andi     $t0, $a3, EUEN_LSX
    bnez     $t0, .Lrestore_lsx
    .irp reg,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        fld.d $f\reg, $a0, (32 * \reg)
    .endr
    b        .Lrestore_fp_control
.Lrestore_lasx:
    .irp reg,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        xvld $xr\reg, $a0, (32 * \reg)
    .endr
    b        .Lrestore_fp_control
.Lrestore_lsx:
    .irp reg,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        vld $vr\reg, $a0, (32 * \reg)
    .endr
.Lrestore_fp_control:
    .irp reg,0,1,2,3,4,5,6,7
        ld.d $t0, $a0, ({context_fcc} + 8 * \reg)
        movgr2cf $fcc\reg, $t0
    .endr
    ld.d     $t0, $a0, {context_fcsr}
    movgr2fcsr $fcsr0, $t0
.Lrestore_integer:
    ld.d     $t1, $a0, CONTEXT_PC
    csrwr    $t1, CSR_ERA
    li.w     $t0, CRMD_PAGED
    li.w     $t1, (CRMD_DIRECT | CRMD_PAGED)
    csrxchg  $t0, $t1, CSR_CRMD
    or       $t0, $a0, $zero
    .irp reg, 1,2,3,4,5,6,7,8,9,10,11,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        ld.d $r\reg, $t0, (CONTEXT_GPR + 8 * \reg)
    .endr
    ld.d     $t0, $t0, (CONTEXT_GPR + 8 * 12)
    ertn

.Lkernel_resume:
    csrrd    $t0, CSR_EUEN
    andi     $t0, $t0, EUEN_FPE
    beqz     $t0, .Lkernel_resume_csrs
    .irp reg,24,25,26,27,28,29,30,31
        fld.d $f\reg, $sp, (FRAME_FPRS + 8 * (\reg - 24))
    .endr
    ld.d     $t0, $sp, FRAME_FCSR
    movgr2fcsr $fcsr0, $t0
.Lkernel_resume_csrs:
    restore_csr CSR_EUEN, FRAME_EUEN
    restore_csr CSR_CRMD, FRAME_CRMD
    invtlb   0, $zero, $zero
    restore_csr CSR_TLBIDX, FRAME_TLBIDX
    restore_csr CSR_TLBEHI, FRAME_TLBEHI
    restore_csr CSR_TLBELO0, FRAME_TLBELO0
    restore_csr CSR_TLBELO1, FRAME_TLBELO1
    restore_csr CSR_STLBPS, FRAME_STLBPS
    restore_csr CSR_DMW0, FRAME_DMW0
    restore_csr CSR_DMW1, FRAME_DMW1
    restore_csr CSR_DMW2, FRAME_DMW2
    restore_csr CSR_DMW3, FRAME_DMW3
    restore_csr CSR_PRMD, FRAME_PRMD
    restore_csr CSR_ERA, FRAME_ERA
    .irp reg, 21,22,23,24,25,26,27,28,29,30,31
        ld.d $r\reg, $sp, (FRAME_R21 + 8 * (\reg - 21))
    .endr
    ld.d     $tp, $sp, FRAME_TP
    ld.d     $ra, $sp, FRAME_RA
    addi.d   $sp, $sp, FRAME_SIZE
    li.w     $a0, 1
    ret

.Lprobe_unavailable:
    or       $a0, $zero, $zero
    ret
.size loongarch64_enter_user, . - loongarch64_enter_user

.balign PAGE_SIZE
.Lexception_entry:
    csrwr    $t0, CSR_SAVE1
    or       $t0, $zero, $zero
    csrwr    $t0, CSR_SAVE0
    beqz     $t0, .Lexception_fatal
    st.d     $t1, $t0, FRAME_USER_T1
    ld.d     $t1, $t0, FRAME_CONTEXT
    .irp reg, 1,2,3,4,5,6,7,8,9,10,11,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        st.d $r\reg, $t1, (CONTEXT_GPR + 8 * \reg)
    .endr
    or       $t2, $zero, $zero
    csrwr    $t2, CSR_SAVE1
    st.d     $t2, $t1, (CONTEXT_GPR + 8 * 12)
    ld.d     $t2, $t0, FRAME_USER_T1
    st.d     $t2, $t1, (CONTEXT_GPR + 8 * 13)
    csrrd    $t4, CSR_ERA
    st.d     $t4, $t1, CONTEXT_PC

    csrrd    $t1, CSR_ESTAT
    bstrpick.d $t2, $t1, 30, 16
    ld.d     $t5, $t0, FRAME_CONTEXT
    st.d     $t2, $t5, {context_exception}
    csrrd    $t3, CSR_PRMD
    andi     $t3, $t3, PRMD_MODE_MASK
    ld.d     $t6, $t0, FRAME_SYSCALL
    beqz     $t6, .Ltimer_exception
    li.w     $t5, {init_traps}
    beq      $t6, $t5, .Linit_exception

    li.w     $t5, PLV3_MASKED
    bne      $t3, $t5, .Lexception_fatal
    li.w     $t5, ESTAT_SYSCALL
    bne      $t2, $t5, .Lexception_fatal
    bne      $t4, $t6, .Lexception_fatal
    b        .Lreturn_to_kernel

.Linit_exception:
    li.w     $t5, PLV3_IRQ_ON
    bne      $t3, $t5, .Lexception_fatal
    beqz     $t2, .Linit_timer_irq
    li.w     $t5, ESTAT_SYSCALL
    beq      $t2, $t5, .Lstop_timer
    ld.d     $t5, $t0, FRAME_CONTEXT
    csrrd    $t6, CSR_BADV
    st.d     $t6, $t5, {context_fault_address}
    b        .Lstop_timer

.Linit_timer_irq:
    andi     $t1, $t1, TIMER_IRQ
    bnez     $t1, .Lstop_timer
    b        .Lexception_fatal

.Ltimer_exception:
    bnez     $t2, .Lexception_fatal
    andi     $t1, $t1, TIMER_IRQ
    beqz     $t1, .Lexception_fatal
    li.w     $t5, PLV3_IRQ_ON
    bne      $t3, $t5, .Lexception_fatal
    user_address $t5, .Labi_wait, $t6
    bltu     $t4, $t5, .Lexception_fatal
    user_address $t5, .Labi_wait_end, $t6
    bgeu     $t4, $t5, .Lexception_fatal

.Lstop_timer:
    or       $t1, $zero, $zero
    csrwr    $t1, CSR_ECFG
    ld.d     $t1, $t0, FRAME_TCFG
    csrwr    $t1, CSR_TCFG
    li.w     $t1, TIMER_CLEAR
    csrwr    $t1, CSR_TICLR
    csrrd    $t1, CSR_ECFG
    bnez     $t1, .Lexception_fatal
    csrrd    $t1, CSR_TCFG
    andi     $t1, $t1, TIMER_ENABLE
    bnez     $t1, .Lexception_fatal
    csrrd    $t1, CSR_ESTAT
    andi     $t1, $t1, TIMER_IRQ
    bnez     $t1, .Lexception_fatal

.Lreturn_to_kernel:
    ld.d     $t1, $t0, FRAME_CONTEXT
    csrrd    $t3, CSR_EUEN
    andi     $t2, $t3, EUEN_FPE
    beqz     $t2, .Lreturn_to_kernel_integer
    andi     $t2, $t3, EUEN_LASX
    bnez     $t2, .Lsave_lasx
    andi     $t2, $t3, EUEN_LSX
    bnez     $t2, .Lsave_lsx
    .irp reg,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        fst.d $f\reg, $t1, (32 * \reg)
    .endr
    b        .Lsave_fp_control
.Lsave_lasx:
    .irp reg,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        xvst $xr\reg, $t1, (32 * \reg)
    .endr
    b        .Lsave_fp_control
.Lsave_lsx:
    .irp reg,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        vst $vr\reg, $t1, (32 * \reg)
    .endr
.Lsave_fp_control:
    .irp reg,0,1,2,3,4,5,6,7
        movcf2gr $t2, $fcc\reg
        st.d $t2, $t1, ({context_fcc} + 8 * \reg)
    .endr
    movfcsr2gr $t2, $fcsr0
    st.d     $t2, $t1, {context_fcsr}
.Lreturn_to_kernel_integer:
    or       $sp, $t0, $zero
    or       $t0, $zero, $zero
    csrwr    $t0, CSR_PRMD
    la.local $t0, .Lkernel_resume
    csrwr    $t0, CSR_ERA
    ertn

.Lexception_fatal:
    csrrd    $a0, CSR_ESTAT
    csrrd    $a1, CSR_ERA
    csrrd    $a2, CSR_BADV
    or       $a3, $zero, $zero
    b        .Lfatal

.balign PAGE_SIZE
.Lrefill_entry:
    csrwr    $t0, CSR_TLBRSAVE
    csrrd    $t0, CSR_SAVE0
    beqz     $t0, .Lrefill_fatal
    st.d     $t1, $t0, FRAME_REFILL_TEMP
    st.d     $t2, $t0, (FRAME_REFILL_TEMP + 8)
    st.d     $t3, $t0, (FRAME_REFILL_TEMP + 16)
    st.d     $t4, $t0, (FRAME_REFILL_TEMP + 24)
    csrrd    $t1, CSR_TLBRPRMD
    andi     $t1, $t1, PLV3_MASKED
    li.w     $t2, PLV3_MASKED
    bne      $t1, $t2, .Lrefill_fatal
    csrrd    $t1, CSR_TLBRBADV
    srli.d   $t4, $t1, 47
    bnez     $t4, .Lrefill_unmapped
    ld.d     $t2, $t0, FRAME_PAGE_ROOT
    .irp shift,39,30,21
        srli.d   $t3, $t1, \shift
        andi     $t3, $t3, 0x1ff
        alsl.d   $t3, $t3, $t2, 3
        ld.d     $t2, $t3, 0
        beqz     $t2, .Lrefill_unmapped
    .endr
    srli.d   $t3, $t1, PAGE_SHIFT
    andi     $t3, $t3, 0x1ff
    alsl.d   $t3, $t3, $t2, 3
    ld.d     $t4, $t3, 0
    andi     $t4, $t4, PTE_VALID
    beqz     $t4, .Lrefill_unmapped
    bstrins.d $t3, $zero, 3, 0
    ld.d     $t2, $t3, 0
    csrwr    $t2, CSR_TLBRELO0
    ld.d     $t2, $t3, 8
    csrwr    $t2, CSR_TLBRELO1
    bstrins.d $t1, $zero, (PAGE_SHIFT), 0
    ori      $t1, $t1, PAGE_SHIFT
    csrwr    $t1, CSR_TLBREHI
    tlbfill
    ld.d     $t1, $t0, FRAME_REFILL_TEMP
    ld.d     $t2, $t0, (FRAME_REFILL_TEMP + 8)
    ld.d     $t3, $t0, (FRAME_REFILL_TEMP + 16)
    ld.d     $t4, $t0, (FRAME_REFILL_TEMP + 24)
    csrrd    $t0, CSR_TLBRSAVE
    ertn

.Lrefill_unmapped:
    ld.d     $t1, $t0, FRAME_SYSCALL
    li.w     $t2, {init_traps}
    bne      $t1, $t2, .Lrefill_fatal
    or       $t1, $zero, $zero
    csrwr    $t1, CSR_SAVE0
    ld.d     $t1, $t0, FRAME_CONTEXT
    csrrd    $t2, CSR_TLBRBADV
    st.d     $t2, $t1, {context_fault_address}
    csrrd    $t2, CSR_TLBRERA
    bstrins.d $t2, $zero, 1, 0
    st.d     $t2, $t1, CONTEXT_PC
    csrwr    $t2, CSR_TLBRERA
    li.w     $t2, {tlb_miss}
    st.d     $t2, $t1, {context_exception}
    b        .Lstop_timer

.Lrefill_fatal:
    or       $a0, $zero, $zero
    csrrd    $a1, CSR_TLBRERA
    bstrins.d $a1, $zero, 1, 0
    csrrd    $a2, CSR_TLBRBADV
    li.w     $a3, 1

.Lfatal:
    or       $t0, $zero, $zero
    csrwr    $t0, CSR_ECFG
    or       $t0, $zero, $zero
    csrwr    $t0, CSR_TCFG
    li.w     $t0, TIMER_CLEAR
    csrwr    $t0, CSR_TICLR
    li.w     $t0, CRMD_DIRECT
    csrwr    $t0, CSR_CRMD
    csrrd    $sp, CSR_FATAL_SP
    b        loongarch64_fatal_trap
