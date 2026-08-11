//! JITビュー (F1a) の検査 — ブロックの切り出しが「認めた語彙だけ、
//! 認めない命令の手前まで」になっているか。

use rustx86_core::jit::{self, JitOp};
use rustx86_core::{Machine, MachineProfile};

fn machine_with_code(code: &[u8], pa: u32) -> Machine {
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(4));
    for (i, b) in code.iter().enumerate() {
        m.write_phys8(pa + i as u32, *b);
    }
    m
}

#[test]
fn reg_only_block_until_branch() {
    // mov eax,5 / mov ebx,eax / add eax,ebx / sub eax,1 / test eax,eax / jne -12
    let code = [
        0xB8, 0x05, 0x00, 0x00, 0x00, // mov eax, 5
        0x89, 0xC3, // mov ebx, eax (89: rm=dst)
        0x01, 0xD8, // add eax, ebx
        0x83, 0xE8, 0x01, // sub eax, 1 (0x83 符号拡張imm8)
        0x85, 0xC0, // test eax, eax
        0x75, 0xF4, // jne -12
    ];
    let m = machine_with_code(&code, 0x10000);
    let b = jit::collect_block(&m, 0x10000, 32).expect("ブロックが取れる");
    assert_eq!(b.head_pa, 0x10000);
    let ops: Vec<JitOp> = b.ops.iter().map(|&(_, op)| op).collect();
    assert_eq!(
        ops,
        vec![
            JitOp::MovRI { dst: 0, imm: 5 },
            JitOp::MovRR { dst: 3, src: 0 },
            JitOp::AluRR {
                kind: 0,
                dst: 0,
                src: 3
            },
            JitOp::AluRI {
                kind: 5,
                dst: 0,
                imm: 1
            },
            JitOp::TestRR { a: 0, b: 0 },
            JitOp::Jcc {
                cc: 5,
                rel: 0xFFFF_FFF4
            },
        ]
    );
    // 命令長も原本どおり (ipの前進は生成コードがこの値で刻む)
    let lens: Vec<u8> = b.ops.iter().map(|&(l, _)| l).collect();
    assert_eq!(lens, vec![5, 2, 2, 3, 2, 2]);
}

#[test]
fn stops_before_memory_op() {
    // mov eax,1 / mov ecx,[ebx] (メモリ形 — JIT対象外) / inc eax
    let code = [
        0xB8, 0x01, 0x00, 0x00, 0x00, // mov eax, 1
        0x8B, 0x0B, // mov ecx, [ebx] ← ここで切れる
        0x40, // inc eax (届かない)
    ];
    let m = machine_with_code(&code, 0x2000);
    let b = jit::collect_block(&m, 0x2000, 32).unwrap();
    assert_eq!(b.ops.len(), 1, "メモリ形の手前までがブロック");
    assert_eq!(b.ops[0].1, JitOp::MovRI { dst: 0, imm: 1 });
}

#[test]
fn none_when_head_is_not_jittable() {
    // 先頭からメモリ形なら焼く物が無い
    let code = [0x8B, 0x03]; // mov eax, [ebx]
    let m = machine_with_code(&code, 0x3000);
    assert!(jit::collect_block(&m, 0x3000, 32).is_none());
}

#[test]
fn layout_addresses_are_distinct_and_stable() {
    let m = machine_with_code(&[0x90], 0x1000);
    let l1 = jit::layout(&m);
    let l2 = jit::layout(&m);
    // 同じMachineなら同じ番地 (生成後に動かない前提の確認)
    assert_eq!(l1.regs, l2.regs);
    assert_eq!(l1.cc_r, l2.cc_r);
    // 全フィールドが別番地
    let all = [
        l1.regs,
        l1.ip,
        l1.flags,
        l1.cc_op,
        l1.cc_w,
        l1.cc_a,
        l1.cc_b,
        l1.cc_cin,
        l1.cc_r,
        l1.tsc,
        l1.tick_countdown,
    ];
    let mut sorted = all.to_vec();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), all.len(), "番地が重複している");
}
