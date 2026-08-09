//! 割り込みと例外の配送、そしてリング遷移。
//!
//! CPUの「制御の流れ」を担う。実行ユニット (alu/shift/...) が値を作るのに対し、
//! ここは**どこへ飛ぶか**を決める: 割り込みゲートを引き、特権が変わるなら
//! スタックを差し替え、iretで元へ戻す。モードで作法が丸ごと変わる
//! (リアルモードのIVT vs 保護モードのIDTゲート) のもここに集まっている。

use super::operand::{pop16, pop32, push16, push32};
use super::*;
use crate::Machine;

/// 割り込み・例外の共通入口。**実IVTを引いてハンドラへ飛ぶ**。
///
/// ソフトウェア割り込み (`INT n`)、例外 (ゼロ除算など)、ハードウェア割り込み
/// (PICからのIRQ) は入口が違うだけで、ここから先は同じ道を通る。
///
/// 積む順序が `CALL far` と違う点に注意: **FLAGSを先に積む**。`IRET` が
/// 逆順に取り出すので、ハンドラ実行中に変わったIF/DFが呼び出し前へ戻る。
pub fn interrupt(m: &mut Machine, n: u8) {
    let (cs, ip) = (m.cpu.sregs[CS], m.cpu.ip);
    let i = n as usize;
    if m.int_counts[i] == 0 {
        m.int_first[i] = (cs, ip);
    }
    m.int_counts[i] += 1;
    if m.int_recent.len() == 32 {
        m.int_recent.pop_front();
    }
    m.int_recent.push_back((n, cs, ip));

    // ここからモードで作法が分かれる。
    if m.cpu.pe() {
        interrupt_protected(m, n);
        return;
    }

    // --- リアルモード: IVT (0番地の 4バイト×256) を引く ---
    let f = (m.cpu.flags as u16) | 0xF002;
    push16(m, f);
    // ハンドラ実行中は多重割り込みとシングルステップを止める。
    // 必要ならハンドラ側が STI で開け直す (これが「割り込み禁止区間」の正体)
    m.cpu.set_flag(IF, false);
    m.cpu.set_flag(TF, false);
    let cs = m.cpu.sregs[CS];
    push16(m, cs);
    let ip = m.cpu.ip as u16;
    push16(m, ip);
    // IVTは 0x0000 から 4バイト × 256個。n番目に [オフセット, セグメント] が並ぶ。
    // **OSはここを自分のハンドラで書き換えて割り込みを乗っ取る**
    let vec = n as u32 * 4;
    m.cpu.ip = m.read16(vec) as u32;
    m.cpu.sregs[CS] = m.read16(vec + 2);
}

/// ソフトウェア INT n の入口。**門のDPLがCPLより浅ければ通さない** —
/// リング3が好きなベクタを叩けたら、保護は成立しない。
/// ハードウェア割り込みと例外はこのチェックを受けない (CPU自身が起こすため)
pub(crate) fn software_int(m: &mut Machine, n: u8) {
    if m.cpu.pe() {
        let off = n as u32 * 8;
        if off + 7 <= m.cpu.idtr_limit as u32 {
            // ゲート記述子の読みは暗黙のスーパーバイザアクセス
            let prev_sys = m.sys_access.replace(true);
            let hi = m.read32(m.cpu.idtr_base.wrapping_add(off).wrapping_add(4));
            m.sys_access.set(prev_sys);
            let gate_dpl = ((hi >> 13) & 3) as u8;
            if gate_dpl < m.cpu.cpl() {
                panic!(
                    "int {n:#04x} from CPL{}: gate DPL={gate_dpl} —                      general protection (まだ#GP配送は無いのでpanic)",
                    m.cpu.cpl()
                );
            }
        }
    }
    interrupt(m, n);
}

/// 保護モードの割り込み配送。IVTではなく**IDTのゲート記述子**を引く。
///
/// ゲートは「どのセグメントの、どこへ、どの作法で」を全部言う8バイト:
///
/// ```text
///   dw offset[15:0]   dw selector   db 0   db type   dw offset[31:16]
/// ```
///
/// type 0xE = 割り込みゲート (IFを落として入る) / 0xF = トラップゲート
/// (IFはそのまま)。この1bitの違いが「割り込みハンドラは再入しない」を
/// ハードウェアで作っている。
///
/// まだやらないこと (リング0だけの世界なので恒真):
/// DPLチェック、スタック切り替え (TSS)、エラーコードのpush
pub(crate) fn interrupt_protected(m: &mut Machine, n: u8) {
    interrupt_protected_err(m, n, None)
}

/// エラーコード付きの配送 (#PF/#GP等)。エラーコードは **EIPの後に** 積まれ、
/// ハンドラの `add $4, %esp` (またはpop) が引き取る約束
pub(crate) fn interrupt_protected_err(m: &mut Machine, n: u8, err: Option<u32>) {
    // 配送はCPUの内部動作 — IDT/GDT/TSSの読みも、切り替えた先の
    // カーネルスタックへのpushも、CPL=3のさなかでもスーパーバイザ権限で行う
    let prev_sys = m.sys_access.replace(true);
    interrupt_protected_inner(m, n, err);
    m.sys_access.set(prev_sys);
}

fn interrupt_protected_inner(m: &mut Machine, n: u8, err: Option<u32>) {
    let off = n as u32 * 8;
    if off + 7 > m.cpu.idtr_limit as u32 {
        panic!(
            "vector {n:#04x} is beyond IDT limit {:#06x}",
            m.cpu.idtr_limit
        );
    }
    let a = m.cpu.idtr_base.wrapping_add(off);
    let lo = m.read32(a);
    let hi = m.read32(a.wrapping_add(4));
    let ty = ((hi >> 8) & 0x1F) as u8;
    if hi & 0x8000 == 0 {
        panic!("vector {n:#04x}: gate not present");
    }
    let (sel, dest) = ((lo >> 16) as u16, (lo & 0xFFFF) | (hi & 0xFFFF_0000));
    match ty {
        0x0E | 0x0F => {}
        _ => panic!("vector {n:#04x}: unimplemented gate type {ty:#04x}"),
    }

    // 受け手のコードセグメントのDPLが、いまより深ければ**リングが変わる**
    let old_cpl = m.cpu.cpl();
    let target_dpl = {
        let off = (sel & !0x7) as u32;
        let a = m.cpu.gdtr_base.wrapping_add(off);
        ((m.read32(a.wrapping_add(4)) >> 13) & 3) as u8
    };

    if target_dpl < old_cpl {
        // ---- リング遷移 (3→0など): スタックを差し替えてから積む ----
        //
        // ここが**TSSの存在理由のすべて**である。リング3のスタックを
        // カーネルが信用するわけにはいかない (ユーザーが好きな場所を
        // 指させられる) ので、落ちた瞬間に使うスタックはTSSが決めておく。
        // 元の SS:ESP は新しいスタックに積んで、帰り道 (iretd) が拾う
        let old_ss = m.cpu.sregs[SS] as u32;
        let old_esp = m.cpu.regs[SP];
        // 32bit TSS: +4 = ESP0, +8 = SS0 (リング0に落ちる場合)
        let esp0 = m.read32(m.cpu.tr_base.wrapping_add(4));
        let ss0 = m.read16(m.cpu.tr_base.wrapping_add(8));
        load_seg_raw(m, SS, ss0);
        m.cpu.regs[SP] = esp0;
        push32(m, old_ss);
        push32(m, old_esp);
    }

    // EFLAGS, CS, EIP を32bitで積む (32bitゲート)
    push32(m, m.cpu.flags);
    push32(m, m.cpu.sregs[CS] as u32);
    push32(m, m.cpu.ip);
    if let Some(e) = err {
        push32(m, e);
    }
    if ty == 0x0E {
        // 割り込みゲートだけがIFを落とす
        m.cpu.set_flag(IF, false);
    }
    m.cpu.set_flag(TF, false);
    load_seg_raw(m, CS, sel);
    m.cpu.set_ip(dest);
}

/// 割り込みからの復帰。IP・CS・FLAGS をこの順で取り出す
pub fn iret(m: &mut Machine) {
    // 保護モード (32bitゲートで入った) なら EIP, CS, EFLAGS を32bitで取り出す。
    // 積む側 (interrupt_protected) と対でなければスタックが腐る
    if m.cpu.pe() {
        let ip = pop32(m);
        let sel = pop32(m) as u16;
        let f = pop32(m);
        // 戻り先のRPLがいまのCPLより浅い (数字が大きい) なら**外側リングへの
        // 復帰**で、ESPとSSもスタックから取り出す。積む側 (リング遷移) と対。
        // 「行ったことのない場所へ戻る」— リング3への降下もこの経路を使う
        let to_outer = ((sel & 3) as u8) > m.cpu.cpl();
        // **popは全部、旧特権のうちに済ませる。** 実機のリング遷移は命令の
        // 最後に一括で完成する — CSを先に積むと、その瞬間CPL=3になり、
        // 残りのpop (カーネルスタック=US=0のページ) がU/S検査で弾かれて
        // ゴミを拾う (Linuxの最初のユーザー空間復帰がこれで死んだ)
        let outer = if to_outer {
            let esp = pop32(m);
            let ss = pop32(m) as u16;
            Some((esp, ss))
        } else {
            None
        };
        load_seg_raw(m, CS, sel);
        m.cpu.set_ip(ip);
        // 復元するフラグの範囲は POPFD と同じ (IOPL/NT/AC/ID まで)
        m.cpu.flags = (f & 0x0024_7FD5) | 0x0002;
        if let Some((esp, ss)) = outer {
            load_seg_raw(m, SS, ss);
            m.cpu.regs[SP] = esp;
        }
        return;
    }
    m.cpu.ip = pop16(m) as u32;
    m.cpu.sregs[CS] = pop16(m);
    let f = pop16(m);
    m.cpu.flags = (f as u32 & 0x0FD5) | 0x0002;
}

/// ページフォールト (#PF, vector 14) の配送。
///
/// **フォールトは命令の先頭IPで配る** (呼び出し側が巻き戻してから来る)。
/// CR2 には失敗した線形アドレスが既に入っている。
/// エラーコード: bit0=P (保護違反=1/不在=0) bit1=W (書き込み) bit2=U (ユーザー)
pub fn page_fault(m: &mut Machine, err: u32) {
    let (cs, ip) = (m.cpu.sregs[CS], m.cpu.ip);
    if m.first_fault.is_none() {
        m.first_fault = Some((14, cs, ip));
    }
    m.int_counts[14] += 1;
    if m.int_recent.len() == 32 {
        m.int_recent.pop_front();
    }
    m.int_recent.push_back((14, cs, ip));
    interrupt_protected_err(m, 14, Some(err));
}

/// ゼロ除算・商オーバーフローで上がる #DE (INT 0)。
///
/// **フォールトなので、積むのは「失敗した命令の先頭」**である。次の命令ではない。
/// ハンドラが原因を直して `IRET` すれば同じ除算をやり直せる、という設計。
/// (8086は次の命令を積む実装だったが、286以降で今の形に直された)
pub(crate) fn divide_error(m: &mut Machine, start_ip: u32) {
    m.cpu.ip = start_ip;
    if m.first_fault.is_none() {
        m.first_fault = Some((0, m.cpu.sregs[CS], start_ip));
    }
    interrupt(m, 0);
}
