.section .text._start, "ax"
.global _start
_start:
    b .Lboot
    .long 0
    .quad 0x200000
    .quad __image_size
    .quad 0, 0, 0, 0
    .long 0x644d5241
    .long 0
.Lboot:
    mov x19, x0
    msr daifset, #0xf
    mrs x0, CurrentEL
    cmp x0, #8
    b.ne 1f
    mov x0, #(1 << 31)
    msr hcr_el2, x0
    msr cptr_el2, xzr
    adr x0, 1f
    msr elr_el2, x0
    mov x0, #0x3c5
    msr spsr_el2, x0
    eret
1:
    mrs x0, CurrentEL
    cmp x0, #4
    b.ne 4f
    msr spsel, #1
    mov x0, #(3 << 20)
    msr cpacr_el1, x0
    msr tpidrro_el0, xzr
    isb
    adrp x0, __bss_start
    add x0, x0, :lo12:__bss_start
    adrp x1, __bss_end
    add x1, x1, :lo12:__bss_end
2:
    cmp x0, x1
    b.hs 3f
    stp xzr, xzr, [x0], #16
    b 2b
3:
    adrp x0, nova_aarch64_stack_top
    add x0, x0, :lo12:nova_aarch64_stack_top
    mov sp, x0
    mov x0, x19
    b bringup_entry
4:
    wfi
    b 4b

.section .bss.kernel_stack, "aw", %nobits
.balign 16
.skip {stack_size}
nova_aarch64_stack_top:

.section .text.aarch64_vectors, "ax"
.balign 2048
.global nova_aarch64_vectors
nova_aarch64_vectors:
.rept 8
    b nova_aarch64_unexpected
    .balign 128
.endr
    b nova_aarch64_user_sync
    .balign 128
    b nova_aarch64_user_irq
    .balign 128
.rept 6
    b nova_aarch64_unexpected
    .balign 128
.endr

nova_aarch64_unexpected:
    mrs x0, esr_el1
    mrs x1, elr_el1
    mrs x2, far_el1
    b nova_aarch64_fatal

.global nova_aarch64_enter_user
nova_aarch64_enter_user:
    stp x19, x20, [sp, #-192]!
    stp x21, x22, [sp, #16]
    stp x23, x24, [sp, #32]
    stp x25, x26, [sp, #48]
    stp x27, x28, [sp, #64]
    stp x29, x30, [sp, #80]
    str x0, [sp, #96]
    stp d8, d9, [sp, #112]
    stp d10, d11, [sp, #128]
    stp d12, d13, [sp, #144]
    stp d14, d15, [sp, #160]
    mrs x1, fpcr
    mrs x2, fpsr
    stp x1, x2, [sp, #176]
    mov x30, x0
    ldp q0, q1, [x30, #272]
    ldp q2, q3, [x30, #304]
    ldp q4, q5, [x30, #336]
    ldp q6, q7, [x30, #368]
    ldp q8, q9, [x30, #400]
    ldp q10, q11, [x30, #432]
    ldp q12, q13, [x30, #464]
    ldp q14, q15, [x30, #496]
    ldp q16, q17, [x30, #528]
    ldp q18, q19, [x30, #560]
    ldp q20, q21, [x30, #592]
    ldp q22, q23, [x30, #624]
    ldp q24, q25, [x30, #656]
    ldp q26, q27, [x30, #688]
    ldp q28, q29, [x30, #720]
    ldp q30, q31, [x30, #752]
    ldr x0, [x30, #784]
    ldr x1, [x30, #792]
    ldr x2, [x30, #800]
    msr fpcr, x0
    msr fpsr, x1
    msr tpidr_el0, x2
    ldp x0, x1, [x30, #248]
    msr sp_el0, x0
    msr elr_el1, x1
    ldr x0, [x30, #264]
    msr spsr_el1, x0
    ldp x0, x1, [x30]
    ldp x2, x3, [x30, #16]
    ldp x4, x5, [x30, #32]
    ldp x6, x7, [x30, #48]
    ldp x8, x9, [x30, #64]
    ldp x10, x11, [x30, #80]
    ldp x12, x13, [x30, #96]
    ldp x14, x15, [x30, #112]
    ldp x16, x17, [x30, #128]
    ldp x18, x19, [x30, #144]
    ldp x20, x21, [x30, #160]
    ldp x22, x23, [x30, #176]
    ldp x24, x25, [x30, #192]
    ldp x26, x27, [x30, #208]
    ldp x28, x29, [x30, #224]
    ldr x30, [x30, #240]
    eret

nova_aarch64_user_sync:
    stp x0, x1, [sp, #-16]!
    mrs x1, esr_el1
    b nova_aarch64_save_user

nova_aarch64_user_irq:
    stp x0, x1, [sp, #-16]!
    mov x1, #-1

nova_aarch64_save_user:
    ldr x0, [sp, #112]
    stp q0, q1, [x0, #272]
    stp q2, q3, [x0, #304]
    stp q4, q5, [x0, #336]
    stp q6, q7, [x0, #368]
    stp q8, q9, [x0, #400]
    stp q10, q11, [x0, #432]
    stp q12, q13, [x0, #464]
    stp q14, q15, [x0, #496]
    stp q16, q17, [x0, #528]
    stp q18, q19, [x0, #560]
    stp q20, q21, [x0, #592]
    stp q22, q23, [x0, #624]
    stp q24, q25, [x0, #656]
    stp q26, q27, [x0, #688]
    stp q28, q29, [x0, #720]
    stp q30, q31, [x0, #752]
    stp x2, x3, [x0, #16]
    stp x4, x5, [x0, #32]
    stp x6, x7, [x0, #48]
    stp x8, x9, [x0, #64]
    stp x10, x11, [x0, #80]
    stp x12, x13, [x0, #96]
    stp x14, x15, [x0, #112]
    stp x16, x17, [x0, #128]
    stp x18, x19, [x0, #144]
    stp x20, x21, [x0, #160]
    stp x22, x23, [x0, #176]
    stp x24, x25, [x0, #192]
    stp x26, x27, [x0, #208]
    stp x28, x29, [x0, #224]
    str x30, [x0, #240]
    mrs x2, sp_el0
    mrs x3, elr_el1
    stp x2, x3, [x0, #248]
    mrs x2, spsr_el1
    str x2, [x0, #264]
    mrs x2, fpcr
    mrs x3, fpsr
    str x2, [x0, #784]
    str x3, [x0, #792]
    mrs x2, tpidr_el0
    str x2, [x0, #800]
    ldp x2, x3, [sp], #16
    stp x2, x3, [x0]
    mov x0, x1
    ldp x2, x3, [sp, #176]
    msr fpcr, x2
    msr fpsr, x3
    ldp d14, d15, [sp, #160]
    ldp d12, d13, [sp, #144]
    ldp d10, d11, [sp, #128]
    ldp d8, d9, [sp, #112]
    ldp x29, x30, [sp, #80]
    ldp x27, x28, [sp, #64]
    ldp x25, x26, [sp, #48]
    ldp x23, x24, [sp, #32]
    ldp x21, x22, [sp, #16]
    ldp x19, x20, [sp], #192
    ret
