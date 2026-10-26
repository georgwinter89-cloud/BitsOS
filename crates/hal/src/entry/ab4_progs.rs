//! AB-4's ktest programs (threads, fork, execve without libc), in the same form as AB-2's in `entry.rs`:
//! position-independent machine code in .rodata that a test copies into a program image. Each ends in
//! `exit_group(code)`; 0 means every check passed, other values name the failed check.

/// Raw clone threads, chosen by argc. 1: three threads spinning in user mode, the main thread spins too (for a
/// kill). 2: a thread sleeps 20 ms and calls exit_group(7) while the main thread sleeps. 3: a thread with
/// CLONE_PARENT_SETTID|CLONE_CHILD_CLEARTID on one word sleeps 20 ms and exit(2)s alone; the main thread
/// checks the word held the thread's id (code 4), waits up to 5 s for the kernel to zero it (code 6), then
/// ends with 0. Code 1: clone failed.
pub fn thread_maker() -> &'static [u8] {
    imp::thread_maker()
}

/// fork: the parent touches 16 stack pages and forks; the child touches 256 more and ends with exit_group(5);
/// the parent sleeps 1 s without waiting (the child is a zombie meanwhile), then wait4(-1) must give the
/// child's pid (code 2) and status 5 << 8 (code 3). Code 1: fork failed. With an argument (argc >= 2) the
/// parent waits at once, without the sleep (the leak test's rounds).
pub fn zombie_maker() -> &'static [u8] {
    imp::zombie_maker()
}

/// execve(argv[1], &argv[1], envp). Returns only on an error: exit 100 + errno. 99: no argv[1].
pub fn exec_arg() -> &'static [u8] {
    imp::exec_arg()
}

#[cfg(target_os = "none")]
mod imp {
    macro_rules! blob {
        ($f:ident, $start:ident, $end:ident) => {
            pub fn $f() -> &'static [u8] {
                unsafe extern "C" {
                    static $start: u8;
                    static $end: u8;
                }
                // SAFETY: both symbols are labels of the same .rodata block (the global_asm below), end after
                // start; the bytes between them are constant and live for the whole kernel run.
                unsafe {
                    let s = &raw const $start;
                    let e = &raw const $end;
                    core::slice::from_raw_parts(s, e as usize - s as usize)
                }
            }
        };
    }
    blob!(thread_maker, hal_ab4_threads_start, hal_ab4_threads_end);
    blob!(zombie_maker, hal_ab4_zombie_start, hal_ab4_zombie_end);
    blob!(exec_arg, hal_ab4_exec_start, hal_ab4_exec_end);
}

#[cfg(not(target_os = "none"))]
mod imp {
    pub fn thread_maker() -> &'static [u8] {
        &[]
    }
    pub fn zombie_maker() -> &'static [u8] {
        &[]
    }
    pub fn exec_arg() -> &'static [u8] {
        &[]
    }
}

// The thread flags of musl's pthread_create without TLS and tid words: VM|FS|FILES|SIGHAND|THREAD|SYSVSEM.
#[cfg(target_os = "none")]
core::arch::global_asm!(
    r#"
    .pushsection .rodata.hal_ab4_programs, "a"

    .macro AB4_EXIT
    xorl %r15d, %r15d
9:
    movl $231, %eax
    movl %r15d, %edi
    syscall
    ud2
    .endm

    .macro AB4_SLEEP_NS ns
    subq $16, %rsp
    movq $0, (%rsp)
    movq $\ns, 8(%rsp)
    movl $35, %eax
    movq %rsp, %rdi
    xorl %esi, %esi
    syscall
    addq $16, %rsp
    .endm

    .global hal_ab4_threads_start
    .global hal_ab4_threads_end
hal_ab4_threads_start:
    movq (%rsp), %rbx
    cmpq $2, %rbx
    je .Lab4_eg
    cmpq $3, %rbx
    je .Lab4_ct
    # 1: three spinning threads, each on its own 64 KiB piece of this stack.
    movl $3, %r12d
    movq %rsp, %r13
1:
    subq $0x10000, %r13
    movl $56, %eax
    movl $0x50f00, %edi
    movq %r13, %rsi
    xorl %edx, %edx
    xorl %r10d, %r10d
    xorl %r8d, %r8d
    syscall
    testq %rax, %rax
    jz .Lab4_spin
    js .Lab4_fail
    decl %r12d
    jnz 1b
.Lab4_spin:
    jmp .Lab4_spin
.Lab4_eg:
    # 2: a thread ends the whole group with 7.
    movl $56, %eax
    movl $0x50f00, %edi
    leaq -0x10000(%rsp), %rsi
    xorl %edx, %edx
    xorl %r10d, %r10d
    xorl %r8d, %r8d
    syscall
    testq %rax, %rax
    js .Lab4_fail
    jz .Lab4_eg_child
2:
    AB4_SLEEP_NS 10000000
    jmp 2b
.Lab4_eg_child:
    AB4_SLEEP_NS 20000000
    movl $231, %eax
    movl $7, %edi
    syscall
    ud2
.Lab4_ct:
    # 3: the kernel zeroes the CLONE_CHILD_CLEARTID word when the thread ends.
    subq $64, %rsp
    movl $0, (%rsp)
    movl $56, %eax
    movl $0x350f00, %edi
    leaq -0x10000(%rsp), %rsi
    movq %rsp, %rdx
    movq %rsp, %r10
    xorl %r8d, %r8d
    syscall
    testq %rax, %rax
    js .Lab4_fail
    jz .Lab4_ct_child
    movl $4, %r15d
    cmpl %eax, (%rsp)
    jne 9f
    movl $5000, %r12d
3:
    cmpl $0, (%rsp)
    je .Lab4_ok
    AB4_SLEEP_NS 1000000
    decl %r12d
    jnz 3b
    movl $6, %r15d
    jmp 9f
.Lab4_ct_child:
    AB4_SLEEP_NS 20000000
    movl $60, %eax
    xorl %edi, %edi
    syscall
    ud2
.Lab4_fail:
    movl $1, %r15d
    jmp 9f
.Lab4_ok:
    AB4_EXIT
hal_ab4_threads_end:

    .global hal_ab4_zombie_start
    .global hal_ab4_zombie_end
hal_ab4_zombie_start:
    movq (%rsp), %r14
    movq %rsp, %rbx
    movl $16, %ecx
1:
    subq $4096, %rbx
    movb $1, (%rbx)
    decl %ecx
    jnz 1b
    movl $57, %eax
    syscall
    testq %rax, %rax
    js .Lab4z_fail
    jz .Lab4z_child
    movq %rax, %r12
    cmpq $2, %r14
    jae 4f
    AB4_SLEEP_NS 999999999
4:
    subq $16, %rsp
    movl $0, (%rsp)
    movl $61, %eax
    movq $-1, %rdi
    movq %rsp, %rsi
    xorl %edx, %edx
    xorl %r10d, %r10d
    syscall
    movl $2, %r15d
    cmpq %r12, %rax
    jne 9f
    movl $3, %r15d
    cmpl $0x500, (%rsp)
    jne 9f
    jmp .Lab4z_ok
.Lab4z_child:
    movq %rsp, %rbx
    movl $256, %ecx
2:
    subq $4096, %rbx
    movb $2, (%rbx)
    decl %ecx
    jnz 2b
    movl $231, %eax
    movl $5, %edi
    syscall
    ud2
.Lab4z_fail:
    movl $1, %r15d
    jmp 9f
.Lab4z_ok:
    AB4_EXIT
hal_ab4_zombie_end:

    .global hal_ab4_exec_start
    .global hal_ab4_exec_end
hal_ab4_exec_start:
    movq (%rsp), %rbx
    movl $99, %r15d
    cmpq $2, %rbx
    jb 9f
    movq 16(%rsp), %rdi
    leaq 16(%rsp), %rsi
    leaq 16(%rsp,%rbx,8), %rdx
    movl $59, %eax
    syscall
    negq %rax
    addq $100, %rax
    movl %eax, %r15d
    jmp 9f
    AB4_EXIT
hal_ab4_exec_end:

    .popsection
"#,
    options(att_syntax)
);
