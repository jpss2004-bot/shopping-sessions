.text
.globl shopping_wide_masks
.hidden shopping_wide_masks
.type shopping_wide_masks,@function
shopping_wide_masks:
    # System V x86-64: rdi=input, rsi=output, rdx=positive block count.
    mov eax, 0x2c2c2c2c
    vpbroadcastd zmm1, eax
    mov eax, 0x0a0a0a0a
    vpbroadcastd zmm2, eax
    mov eax, 0x22222222
    vpbroadcastd zmm3, eax
.Lwide_loop:
    vmovdqu64 zmm0, [rdi]
    vpcmpeqb k1, zmm0, zmm1
    vpcmpeqb k2, zmm0, zmm2
    vpcmpeqb k3, zmm0, zmm3
    kmovq [rsi], k1
    kmovq [rsi+8], k2
    kmovq [rsi+16], k3
    add rdi, 64
    add rsi, 24
    dec rdx
    jnz .Lwide_loop
    vzeroupper
    ret
.size shopping_wide_masks,.-shopping_wide_masks
