.pushsection .nova_user_text, "ax", @progbits
.option push
.option arch, +v
.global nova_riscv64_user_main
.global nova_riscv64_user_spin
.global nova_riscv64_user_spin_end
nova_riscv64_user_main:
    mv s0, a0
    mv s1, a1
    mv s2, a3
    mv tp, a0
    fmv.d.x f31, a0
    csrw fcsr, a1
    mv s3, a4
    beqz s3, .Luser_vector_initialized
    addi t0, s1, 5
    vsetvli zero, t0, e8, m1, tu, mu
    vmv.v.x v0, s0
    vmv.v.x v31, s0
    addi t0, s1, 1
    csrw vcsr, t0
    csrw vstart, t0
.Luser_vector_initialized:
    addi sp, sp, -80
    sd a0, 64(sp)
    li a0, -1
    li a7, {get_version}
    ecall
    li t0, {version}
    bne a0, t0, .Luser_failed
    bnez s1, .Luser_yield

    li t0, {spin_limit}
nova_riscv64_user_spin:
    addi t0, t0, -1
    bnez a2, .Luser_resumed
    bnez t0, nova_riscv64_user_spin
nova_riscv64_user_spin_end:
    j .Luser_failed

.Luser_resumed:
    beqz t0, .Luser_failed
    sd s0, 0(sp)
    mv a0, s2
    li a1, 0x31
    mv a2, sp
    li a3, 8
    li a7, {send}
    ecall
    bnez a0, .Luser_failed
    j .Luser_check_stack

.Luser_yield:
    li a0, -1
    li a7, {yield_thread}
    ecall
    bnez a0, .Luser_failed
    mv a0, s2
    li a1, 0
    mv a2, sp
    li a3, {max_inline_payload}
    li a4, 0
    li a7, {recv}
    ecall
    li t0, 0x31
    bne a0, t0, .Luser_failed
    ld t0, 0(sp)
    addi t1, s0, -1
    bne t0, t1, .Luser_failed

.Luser_check_stack:
    beqz s3, .Luser_vector_checked
    csrr t0, vl
    addi t1, s1, 5
    bne t0, t1, .Luser_failed
    csrr t0, vtype
    bnez t0, .Luser_failed
    addi t1, s1, 1
    csrr t0, vcsr
    bne t0, t1, .Luser_failed
    csrr t0, vstart
    bne t0, t1, .Luser_failed
    csrw vstart, zero
    vmv.x.s t0, v0
    bne t0, s0, .Luser_failed
    vmv.x.s t0, v31
    bne t0, s0, .Luser_failed
.Luser_vector_checked:
    bne tp, s0, .Luser_failed
    fmv.x.d t0, f31
    bne t0, s0, .Luser_failed
    csrr t0, fcsr
    bne t0, s1, .Luser_failed
    ld a0, 64(sp)
    addi sp, sp, 80
    bne a0, s0, .Luser_failed
    j .Luser_exit

.Luser_failed:
    li a0, 0

.Luser_exit:
    li a7, {thread_exit}
    ecall
    unimp
.option pop
.popsection
