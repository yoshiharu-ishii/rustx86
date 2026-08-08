//! Tier 1c で足した命令のco-simテスト。
//!
//! ALU系と違い、ここで検証したいのは**フラグではなく状態の移動**である:
//! CSが変わる (far転送)、セグメントレジスタが変わる (POP Sreg / LES / LDS)、
//! スタックの中身が変わる (PUSHA / ENTER)。
//! そのためハーネス側にセグメントレジスタとスタック観測窓を足してある。

use rustx86_cosim::*;

/// セグメントレジスタのPUSH/POP。
///
/// オペコードのbit3-4がES/CS/SS/DSの番号そのものになっている格子構造で、
/// 実装も `(op >> 3) & 3` の1行に畳んである。格子が正しいかを4本とも叩く。
#[test]
fn segment_push_pop() {
    let templates = vec![
        Template { name: "PUSH ES", undefined: 0, fixup: fix_stack, build: |_| vec![0x06] },
        Template { name: "PUSH CS", undefined: 0, fixup: fix_stack, build: |_| vec![0x0E] },
        Template { name: "PUSH SS", undefined: 0, fixup: fix_stack, build: |_| vec![0x16] },
        Template { name: "PUSH DS", undefined: 0, fixup: fix_stack, build: |_| vec![0x1E] },
        Template { name: "POP ES", undefined: 0, fixup: fix_stack, build: |_| vec![0x07] },
        Template { name: "POP SS", undefined: 0, fixup: fix_stack, build: |_| vec![0x17] },
        Template { name: "POP DS", undefined: 0, fixup: fix_stack, build: |_| vec![0x1F] },
    ];
    check(&templates, 200, 0x5E67_0001);
}

/// PUSHA/POPA/PUSH imm (186で足された命令群)
#[test]
fn push_all_and_imm() {
    let templates = vec![
        // PUSHAが積むSPは「PUSHA開始時点の」値。実装がSP更新後の値を積んでいれば
        // スタック観測窓の該当2バイトが食い違う
        Template { name: "PUSHA", undefined: 0, fixup: fix_stack, build: |_| vec![0x60] },
        Template { name: "POPA", undefined: 0, fixup: fix_stack_low, build: |_| vec![0x61] },
        Template {
            name: "PUSH imm16",
            undefined: 0,
            fixup: fix_stack,
            build: |r| { let v = r.interesting_u16(); vec![0x68, v as u8, (v >> 8) as u8] },
        },
        Template {
            name: "PUSH imm8 (符号拡張)",
            undefined: 0,
            fixup: fix_stack,
            build: |r| vec![0x6A, r.interesting_u8()],
        },
    ];
    check(&templates, 200, 0x9057_0002);
}

/// ENTER/LEAVE: スタックフレームの作成と破棄
#[test]
fn enter_leave() {
    let templates = vec![
        // level=0 が現代のコンパイラが出す唯一の形
        Template {
            name: "ENTER imm16,0",
            undefined: 0,
            fixup: fix_frame,
            build: |r| vec![0xC8, (r.next_u16() as u8) & 0x0E, 0x00, 0x00],
        },
        // level>0 はPascal系のネスト手続き用。display を積むループが動く
        Template {
            name: "ENTER imm16,level",
            undefined: 0,
            fixup: fix_frame,
            build: |r| vec![0xC8, (r.next_u16() as u8) & 0x06, 0x00, (r.next_u16() as u8) % 4],
        },
        Template { name: "LEAVE", undefined: 0, fixup: fix_frame, build: |_| vec![0xC9] },
    ];
    check(&templates, 200, 0xE47E_0003);
}

/// IMUL の3オペランド形式 (186)。
/// CF/OFは「結果が16bitに収まらなかったか」だけを表し、SF/ZF/AF/PFは未定義
#[test]
fn imul_three_operand() {
    const UD: u16 = UD_SF | UD_ZF | UD_AF | UD_PF;
    let templates = vec![
        Template {
            name: "IMUL r16,r/m16,imm16",
            undefined: UD,
            fixup: nofix,
            build: |r| {
                let v = r.interesting_u16();
                vec![0x69, 0xC0 | ((r.next_u16() as u8 & 7) << 3) | (r.next_u16() as u8 & 7), v as u8, (v >> 8) as u8]
            },
        },
        Template {
            name: "IMUL r16,r/m16,imm8",
            undefined: UD,
            fixup: nofix,
            build: |r| {
                vec![0x6B, 0xC0 | ((r.next_u16() as u8 & 7) << 3) | (r.next_u16() as u8 & 7), r.interesting_u8()]
            },
        },
    ];
    check(&templates, 400, 0x1301_0004);
}

/// LES/LDS/XLAT: メモリから複合的な値を取る命令
#[test]
fn far_pointer_load_and_xlat() {
    let templates = vec![
        // far ポインタ (4バイト) を読み、下位2バイトを汎用レジスタ、
        // 上位2バイトをセグメントレジスタへ入れる。ESとDSの取り違えを検出する
        Template {
            name: "LES r16,m",
            undefined: 0,
            fixup: nofix,
            build: |r| {
                let off = DATA_ADDR + (r.next_u16() % 12);
                vec![0xC4, ((r.next_u16() as u8 & 7) << 3) | 0x06, off as u8, (off >> 8) as u8]
            },
        },
        Template {
            name: "LDS r16,m",
            undefined: 0,
            fixup: nofix,
            build: |r| {
                let off = DATA_ADDR + (r.next_u16() % 12);
                vec![0xC5, ((r.next_u16() as u8 & 7) << 3) | 0x06, off as u8, (off >> 8) as u8]
            },
        },
        // XLAT: AL = [BX + AL]。BXを固定してALを振る
        Template { name: "XLAT", undefined: 0, fixup: fix_xlat, build: |_| vec![0xD7] },
    ];
    check(&templates, 300, 0x1E5D_0005);
}

/// far転送: CSごと移る命令。
///
/// リアルモードでは「CSに値を代入する」だけだが、プロテクトモードでは
/// 同じ命令がディスクリプタ引きと特権チェックに化ける (Tier 3)。
/// ここで素の挙動を固めておく。
#[test]
fn far_transfers() {
    let templates = vec![
        // 飛び先セグメントは 0x0000-0x0FFF に抑える。
        // CS:IP が 1MB を超えるとオラクル側が未マップ領域を触るため
        Template {
            name: "JMP far imm",
            undefined: 0,
            fixup: fix_stack,
            build: |r| {
                let off = r.interesting_u16();
                let seg = r.next_u16() & 0x0FFF;
                vec![0xEA, off as u8, (off >> 8) as u8, seg as u8, (seg >> 8) as u8]
            },
        },
        // CALL far は「CSを積んでからIPを積む」。順序を逆にすると
        // スタック観測窓の4バイトが入れ替わって出る
        Template {
            name: "CALL far imm",
            undefined: 0,
            fixup: fix_stack,
            build: |r| {
                let off = r.interesting_u16();
                let seg = r.next_u16() & 0x0FFF;
                vec![0x9A, off as u8, (off >> 8) as u8, seg as u8, (seg >> 8) as u8]
            },
        },
        Template { name: "RETF", undefined: 0, fixup: fix_stack, build: |_| vec![0xCB] },
        Template {
            name: "RETF imm16",
            undefined: 0,
            fixup: fix_stack,
            build: |r| { let n = r.next_u16() & 0x0F; vec![0xCA, n as u8, (n >> 8) as u8] },
        },
        // IRET は CALL far と違い FLAGS も戻す。
        // 割り込み中に変わったIF/DFを呼び出し前へ戻すのが役目
        Template { name: "IRET", undefined: 0, fixup: fix_stack, build: |_| vec![0xCF] },
    ];
    check(&templates, 300, 0xFA12_0006);
}

/// GRP5 の far call/jmp (メモリ上の4バイトポインタ経由)。
///
/// 飛び先セグメントがランダムだとオラクルが未マップ領域を触るため、
/// ランダムテンプレートではなく飛び先を明示したケースで検証する。
#[test]
fn far_transfer_via_memory() {
    let mut cases = Vec::new();
    for (off, seg) in [(0x0000u16, 0x0000u16), (0x1234, 0x0100), (0xFFFF, 0x0FFF), (0x0001, 0x0002)] {
        for kind in [3u8, 5u8] {
            let mut data = [0u8; 16];
            data[0..2].copy_from_slice(&off.to_le_bytes());
            data[2..4].copy_from_slice(&seg.to_le_bytes());
            let mut regs = [0x1111u16, 0x2222, 0x3333, 0x4444, 0, 0x6666, 0x7777, 0x8888];
            fix_stack(&mut regs);
            cases.push(TestCase {
                // FF /kind [disp16] — reg フィールドが 3 なら CALL far、5 なら JMP far
                code: vec![0xFF, (kind << 3) | 0x06, DATA_ADDR as u8, (DATA_ADDR >> 8) as u8],
                regs,
                sregs: [0x0055, 0, 0, 0],
                flags: 0,
                data,
                stack: [0; STACK_WINDOW],
            });
        }
    }
    check_cases("GRP5 far call/jmp [mem]", 0, cases);
}
