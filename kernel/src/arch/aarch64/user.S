.section .user_probe.text, "ax"
.global nova_aarch64_user_fault
nova_aarch64_user_fault:
    str wzr, [x0]
    brk #0

.global nova_aarch64_user_main
nova_aarch64_user_main:
    mov x19, x0
    dup v31.2d, x0
    msr tpidr_el0, x0
    mov x20, x1
    mov x21, x3
    sub sp, sp, #80
    str x0, [sp, #64]
    mov x8, #{get_version}
    svc #0
    movz x1, #{version_minor}
    movk x1, #{version_major}, lsl #16
    cmp x0, x1
    b.ne .Lfailed
    cbnz x20, .Lreceiver
1:
    isb
    mrs x1, cntvct_el0
    cmp x1, x2
    b.lo 1b
    mov x0, x21
    mov x3, #0
    mov x4, #0
    mov x8, #{recv}
    svc #0
    cmn x0, #{denied}
    b.ne .Lfailed
    mov x0, x21
    mov x1, #0x31
    add x2, sp, #79
    mov x3, #2
    mov x8, #{send}
    svc #0
    cmn x0, #{invalid}
    b.ne .Lfailed
    mov x0, x21
    mov x1, #0x31
    mov x2, sp
    mov x3, #8
    str x19, [sp]
    mov x8, #{send}
    svc #0
    cbnz x0, .Lfailed
    mov x0, x21
    mov x1, #0x32
    mov x2, sp
    mov x3, #8
    add x4, x19, #1
    str x4, [sp]
    mov x8, #{send}
    svc #0
    cbnz x0, .Lfailed
    b .Lexit
.Lreceiver:
    mov x8, #{yield_thread}
    svc #0
    cbnz x0, .Lfailed
    mov x0, x21
    mov x1, #0
    adr x2, nova_aarch64_user_main
    mov x3, #{max_inline_payload}
    mov x4, #0
    mov x8, #{recv}
    svc #0
    cmn x0, #{invalid}
    b.ne .Lfailed
    mov x0, x21
    mov x1, #0
    mov x2, sp
    mov x3, #{max_inline_payload}
    mov x4, #0
    mov x8, #{recv}
    svc #0
    cmp x0, #0x31
    b.ne .Lfailed
    ldr x4, [sp]
    sub x5, x19, #1
    cmp x4, x5
    b.ne .Lfailed
    mov x0, x21
    mov x1, #0
    mov x2, sp
    mov x3, #{max_inline_payload}
    mov x4, #0
    mov x8, #{recv}
    svc #0
    cmp x0, #0x32
    b.ne .Lfailed
    ldr x4, [sp]
    cmp x4, x19
    b.eq .Lexit
.Lfailed:
    mov x19, #0
.Lexit:
    umov x4, v31.d[1]
    cmp x4, x19
    b.ne .Lbad_context
    mrs x4, tpidr_el0
    cmp x4, x19
    b.eq .Lexit_syscall
.Lbad_context:
    mov x19, #0
.Lexit_syscall:
    ldr x0, [sp, #64]
    add sp, sp, #80
    cmp x0, x19
    csel x0, x0, xzr, eq
    mov x8, #{thread_exit}
    svc #0
    brk #0
