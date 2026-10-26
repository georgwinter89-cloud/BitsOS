bits 64
; mm.as.user_touch: maps 64 KiB, touches three pages, reports, grows the heap, reports, guards a page, touches it.
    mov eax, 9              ; mmap(0, 0x10000, RW, PRIVATE|ANON, -1, 0)
    xor edi, edi
    mov esi, 0x10000
    mov edx, 3
    mov r10d, 0x22
    mov r8, -1
    xor r9d, r9d
    syscall
    mov rbx, rax
    mov qword [rbx], 0x1111
    mov qword [rbx + 0x8000], 0x2222
    mov rcx, [rbx + 0xF008]
    add rcx, [rbx]
    add rcx, [rbx + 0x8000]
    mov rdi, rcx
    mov rsi, rbx
    mov eax, 1000           ; report(sum, base)
    syscall
    mov eax, 12             ; brk(0)
    xor edi, edi
    syscall
    mov r12, rax
    lea rdi, [rax + 0x3000]
    mov eax, 12             ; brk(cur + 0x3000)
    syscall
    mov byte [r12 + 0x2fff], 0x77
    movzx edi, byte [r12 + 0x2fff]
    mov rsi, r12
    mov eax, 1001           ; report(byte, old break)
    syscall
    lea rdi, [rbx + 0x4000]
    mov esi, 0x1000
    xor edx, edx
    mov eax, 10             ; mprotect(base + 0x4000, 0x1000, PROT_NONE)
    syscall
    mov rax, [rbx + 0x4000] ; faults: PROT_NONE
    mov edi, 1
    mov eax, 60             ; exit(1): never reached
    syscall
