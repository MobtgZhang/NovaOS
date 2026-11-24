.pushsection .text.nova_riscv64_enter_user, "ax", @progbits
.option push
.option norelax
.equ TIMER_ARMED, 152
.equ USER_CONTEXT, 160
.equ USER_T6, 168
.equ KERNEL_FCSR, 176
.equ KERNEL_FPRS, 184
.equ KERNEL_FRAME_SIZE, 288
.macro user_gprs op
    \op ra,   8(t6)
    \op gp,  24(t6)
    \op tp,  32(t6)
    \op t0,  40(t6)
    \op t1,  48(t6)
    \op t2,  56(t6)
    \op s0,  64(t6)
    \op s1,  72(t6)
    \op a0,  80(t6)
    \op a1,  88(t6)
    \op a2,  96(t6)
    \op a3, 104(t6)
    \op a4, 112(t6)
    \op a5, 120(t6)
    \op a6, 128(t6)
    \op a7, 136(t6)
    \op s2, 144(t6)
    \op s3, 152(t6)
    \op s4, 160(t6)
    \op s5, 168(t6)
    \op s6, 176(t6)
    \op s7, 184(t6)
    \op s8, 192(t6)
    \op s9, 200(t6)
    \op s10,208(t6)
    \op s11,216(t6)
    \op t3, 224(t6)
    \op t4, 232(t6)
    \op t5, 240(t6)
.endm
.macro user_fprs op
    .irp reg,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        \op f\reg, ({context_fpr} + 8 * \reg)(t6)
    .endr
.endm
.macro kernel_fprs op
    .irp reg,0,1,2,3,4,5,6,7,8,9,10,11
        \op fs\reg, (KERNEL_FPRS + 8 * \reg)(sp)
    .endr
.endm
.macro vector_registers op
    .option push
    .option arch, +v
    addi t0, t6, {context_vector}
    slli t1, t1, 3
    .irp reg,0,8,16,24
        \op v\reg, (t0)
        add t0, t0, t1
    .endr
    .option pop
.endm
.macro restore_user_vector
    ld t1, {context_vector_bytes}(t6)
    beqz t1, .Lskip_vector_restore
    li t0, {vs}
    csrs sstatus, t0
    csrw vstart, zero
    vector_registers vl8re8.v
    ld t0, {context_vector_length}(t6)
    ld t1, {context_vector_type}(t6)
    .option push
    .option arch, +v
    vsetvl zero, t0, t1
    .option pop
    ld t0, {context_vector_control}(t6)
    csrw vcsr, t0
    ld t0, {context_vector_start}(t6)
    csrw vstart, t0
.Lskip_vector_restore:
.endm
.macro save_user_vector
    ld t1, {context_vector_bytes}(t6)
    beqz t1, .Lskip_vector_save
    csrrw t0, vstart, zero
    sd t0, {context_vector_start}(t6)
    csrr t0, vcsr
    sd t0, {context_vector_control}(t6)
    csrr t0, vl
    sd t0, {context_vector_length}(t6)
    csrr t0, vtype
    sd t0, {context_vector_type}(t6)
    vector_registers vs8r.v
.Lskip_vector_save:
.endm
.macro sbi_set_timer
    li a6, {sbi_set_timer}
    li a7, {sbi_time}
    ecall
.endm
.global nova_riscv64_enter_user
.type nova_riscv64_enter_user, @function
.balign 4
nova_riscv64_enter_user:
    csrr t0, satp
    bnez t0, .Lbad_boot_state
    csrr t0, sie
    bnez t0, .Lbad_boot_state

    addi sp, sp, -KERNEL_FRAME_SIZE
    sd ra,   0(sp)
    sd gp,   8(sp)
    sd tp,  16(sp)
    sd s0,  24(sp)
    sd s1,  32(sp)
    sd s2,  40(sp)
    sd s3,  48(sp)
    sd s4,  56(sp)
    sd s5,  64(sp)
    sd s6,  72(sp)
    sd s7,  80(sp)
    sd s8,  88(sp)
    sd s9,  96(sp)
    sd s10,104(sp)
    sd s11,112(sp)
    csrrci t0, sstatus, {sie}
    sd t0, 120(sp)
    csrr t0, stvec
    sd t0, 128(sp)
    csrrw t0, sscratch, sp
    sd t0, 136(sp)
    csrr t0, sepc
    sd t0, 144(sp)
    sd zero, TIMER_ARMED(sp)
    sd a1, USER_CONTEXT(sp)

    lla t0, .Lprobe_trap
    csrw stvec, t0
    csrw satp, a0
    sfence.vma
    csrr t0, satp
    bne t0, a0, .Lno_sv48

    li t0, {user_status_clear}
    csrc sstatus, t0
    li t0, {fs}
    csrs sstatus, t0
    csrr t0, fcsr
    sd t0, KERNEL_FCSR(sp)
    kernel_fprs fsd
    beqz a2, .Lenter_user

    rdtime a0
    add a0, a0, a2
    sbi_set_timer
    bnez a0, .Ltimer_arm_failed
    li t1, {stie}
    sd t1, TIMER_ARMED(sp)
    csrs sie, t1
    csrr t1, sie
    andi t1, t1, {stie}
    beqz t1, .Ltimer_arm_failed

.Lenter_user:
    ld t6, USER_CONTEXT(sp)
    ld t0, {context_pc}(t6)
    csrw sepc, t0
    user_fprs fld
    ld t0, {context_fcsr}(t6)
    csrw fcsr, t0
    restore_user_vector
    user_gprs ld
    ld sp, 16(t6)
    ld t6, 248(t6)
    sret

.balign 4
.Lprobe_trap:
    csrrw sp, sscratch, sp
    sd t6, USER_T6(sp)
    ld t6, USER_CONTEXT(sp)
    user_gprs sd
    csrr t0, sscratch
    sd t0, 16(t6)
    ld t0, USER_T6(sp)
    sd t0, 248(t6)
    csrr t0, sepc
    sd t0, {context_pc}(t6)

    csrr t0, sstatus
    andi t0, t0, {spp}
    bnez t0, .Lunexpected_trap
    save_user_vector
    user_fprs fsd
    csrr t0, fcsr
    sd t0, {context_fcsr}(t6)
    kernel_fprs fld
    ld t0, KERNEL_FCSR(sp)
    csrw fcsr, t0
    csrr a0, scause
    bgez a0, .Luser_exception
    ld t1, TIMER_ARMED(sp)
    beqz t1, .Lunexpected_trap
    li t1, {supervisor_timer}
    bne a0, t1, .Lunexpected_trap
    j .Lexpected_trap

.Luser_exception:
    csrr t0, stval
    sd t0, {context_fault_address}(t6)

.Lexpected_trap:
    li a2, 0
    j .Lrestore_kernel

.Ltimer_arm_failed:
    li a0, {timer_arm_failed}
    j .Lsetup_error

.Lno_sv48:
    li a0, {no_sv48}
.Lsetup_error:
    li a2, 0
    j .Lrestore_kernel

.Lunexpected_trap:
    li a2, 1

.Lrestore_kernel:
    csrw sie, zero
    csrw satp, zero
    sfence.vma
    ld t0, TIMER_ARMED(sp)
    beqz t0, .Lrestore_context
    mv t2, a0
    li a0, -1
    sbi_set_timer
    beqz a0, .Ltimer_stopped
    li t2, {timer_stop_failed}
.Ltimer_stopped:
    mv a0, t2

.Lrestore_context:
    ld t0, 128(sp)
    csrw stvec, t0
    ld t0, 136(sp)
    csrw sscratch, t0
    bnez a2, .Lrestore_gprs
    ld t0, 144(sp)
    csrw sepc, t0
.Lrestore_gprs:
    ld ra,   0(sp)
    ld gp,   8(sp)
    ld tp,  16(sp)
    ld s0,  24(sp)
    ld s1,  32(sp)
    ld s2,  40(sp)
    ld s3,  48(sp)
    ld s4,  56(sp)
    ld s5,  64(sp)
    ld s6,  72(sp)
    ld s7,  80(sp)
    ld s8,  88(sp)
    ld s9,  96(sp)
    ld s10,104(sp)
    ld s11,112(sp)
    ld t0, 120(sp)
    addi sp, sp, KERNEL_FRAME_SIZE
    csrw sstatus, t0
    bnez a2, .Lfatal
    ret

.Lbad_boot_state:
    li a0, {bad_boot_state}
    ret

.Lfatal:
    tail {fatal}
.size nova_riscv64_enter_user, . - nova_riscv64_enter_user
.purgem user_gprs
.purgem user_fprs
.purgem kernel_fprs
.purgem vector_registers
.purgem restore_user_vector
.purgem save_user_vector
.purgem sbi_set_timer
.popsection

.pushsection .nova_user_text, "ax", @progbits
.global nova_riscv64_cookie_entry
.global nova_riscv64_cookie_ecall
.global nova_riscv64_timer_entry
.global nova_riscv64_timer_spin
.global nova_riscv64_timer_timeout
nova_riscv64_cookie_entry:
    addi sp, sp, -16
    li t0, {cookie}
    sd t0, 0(sp)
    ld a0, 0(sp)
nova_riscv64_cookie_ecall:
    ecall
    unimp

nova_riscv64_timer_entry:
    li a0, {timer_spin_limit}
nova_riscv64_timer_spin:
    addi a0, a0, -1
    bnez a0, nova_riscv64_timer_spin
nova_riscv64_timer_timeout:
    ecall
    unimp
.option pop
.popsection
