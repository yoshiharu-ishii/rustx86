//! ELKS (16bit UNIX) の起動テスト。
//!
//! ディスクイメージはリポジトリに含めない (配布物なので) 。
//! 置いていない環境では**黙って成功にする** — CIで落ちないようにするためだが、
//! 「動かしていないのに緑」になるので、走ったかどうかは標準エラーに出す。
//!
//! ```text
//! curl -sL -o images/fd1440.img \
//!   https://github.com/ghaerr/elks/releases/download/v0.9.1/fd1440.img
//! ```
//!
//! co-simは1命令単位なので、装置の状態遷移も割り込みの受付タイミングも
//! 検証できない。**ここからは実OSを動かすこと自体がテストになる**。

use rustx86_core::Machine;

const IMAGE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../images/fd1440.img");

/// 画面に文字列が出るまで走らせる
fn run_until(m: &mut Machine, needle: &str, budget: u64) -> bool {
    for _ in 0..budget {
        if m.halted && m.pending_irq.is_none() && !m.devices.pit.counters[0].running {
            return false;
        }
        m.step();
        // 画面を毎命令組み立ててはいけない。1命令ごとに80x25文字のStringを
        // 作ることになり、起動が数百倍遅くなる (実際にやって390秒かかった)。
        // Tier 2a で入れた dirty フラグで、**書き換わったときだけ**見る
        if m.take_vram_dirty() && m.text_screen_string().contains(needle) {
            return true;
        }
    }
    false
}

fn boot() -> Option<Machine> {
    let image = std::fs::read(IMAGE).ok()?;
    let mut m = Machine::new();
    m.boot_from_disk(image).expect("boot");
    Some(m)
}

/// カーネルがルートをマウントし、loginプロンプトまで到達する
#[test]
fn elks_boots_to_login_prompt() {
    let Some(mut m) = boot() else {
        eprintln!("images/fd1440.img が無いのでスキップ");
        return;
    };
    assert!(
        run_until(&mut m, "login:", 100_000_000),
        "loginプロンプトに到達せず"
    );
    let screen = m.text_screen_string();
    assert!(
        screen.contains("ELKS 0.9.1"),
        "バージョン表示が無い:\n{screen}"
    );
    assert!(
        screen.contains("Mounted root device"),
        "ルートがマウントされていない:\n{screen}"
    );
}

/// tetris のテンポ — 「予算=仮想時間」の会計をゲームで検証する。
///
/// ELKSのtetris (elkscmd/tui/ttytetris.c) は read(2) のブロックが
/// SIGALRM (setitimer, 300ms) で切られるまで待つことでテンポを作る。
/// つまり **read が -EINTR で戻る回数 = 駒が1段落ちる回数** で、
/// ゲストの時計が正しければ仮想時間3秒で10回前後になる。
///
/// ブラウザ (machine.js) は run_slice をCHUNK=6000刻みで呼び、
/// 「頼んだ分だけ進んだ」と勘定する。アイドルの早送りが予算を飛び越えると
/// この勘定が壊れ、時計が百倍速で流れて駒が一瞬で積み上がった (実際に
/// なった)。ここではそのスライス刻みを忠実に再現して、テンポを数える。
#[test]
fn elks_tetris_tempo() {
    let Some(mut m) = boot() else {
        eprintln!("images/fd1440.img が無いのでスキップ");
        return;
    };
    assert!(
        run_until(&mut m, "login:", 200_000_000),
        "loginプロンプトに到達せず"
    );
    m.devices.keyboard.type_ascii("root\n");
    let ok = run_until(&mut m, "# ", 20_000_000) || {
        let s = m.text_screen_string();
        s.lines().last().map(|l| l.trim() == "#").unwrap_or(false)
    };
    assert!(
        ok,
        "シェルのプロンプトが出ない:\n{}",
        m.text_screen_string()
    );

    m.devices.keyboard.type_ascii("tetris\n");
    for _ in 0..2_000_000 {
        m.step(); // ゲームの初期化 (端末設定・itimer装填) を通す
    }
    assert!(
        m.text_screen_string().contains("Score:"),
        "tetrisが起動していない:\n{}",
        m.text_screen_string()
    );

    // ブラウザのフレームループを模す: 60fps相当の予算をCHUNK刻みで消費。
    // 仮想時間で3秒ぶん。read の入口 (int 0x80, ax=3, bx=0) と戻り番地を
    // 見張り、-EINTR (=-4) の戻り = SIGALRM に切られた回数を数える
    const INSTR_PER_GUEST_MS: u64 = 1_193_182 * 64 / 1000;
    const CHUNK: u64 = 6_000;
    let mut alarms = 0u32;
    let mut ret_watch: Option<(u16, u32)> = None;
    for _frame in 0..180 {
        let budget = 167 * INSTR_PER_GUEST_MS / 10; // 16.7ms
        let mut done = 0u64;
        while done < budget {
            let slice = CHUNK.min(budget - done);
            let start = m.cpu.tsc;
            loop {
                let elapsed = m.cpu.tsc.wrapping_sub(start);
                if elapsed >= slice {
                    break;
                }
                if let Some((cs, rip)) = ret_watch {
                    if m.cpu.sregs[rustx86_core::cpu::CS] == cs && m.cpu.ip == rip {
                        ret_watch = None;
                        // -EINTR (-4) または 0 (ELKSはSIGALRM後のreadにEOFを
                        // 返すことがある — xgetchar が clearerr で拾う仕様)。
                        // どちらも「キーは無く、アラームで切られた」= 1テンポ
                        if (m.cpu.regs[rustx86_core::cpu::AX] as i16) <= 0 {
                            alarms += 1;
                        }
                    }
                }
                let lin = m.cpu.lin(rustx86_core::cpu::CS, m.cpu.ip) as usize;
                if ret_watch.is_none()
                    && lin + 1 < m.mem.len()
                    && m.mem[lin] == 0xCD
                    && m.mem[lin + 1] == 0x80
                    && m.cpu.regs[rustx86_core::cpu::AX] as u16 == 3
                    && m.cpu.regs[rustx86_core::cpu::BX] as u16 == 0
                {
                    ret_watch =
                        Some((m.cpu.sregs[rustx86_core::cpu::CS], m.cpu.ip.wrapping_add(2)));
                }
                m.step_budgeted(slice - elapsed);
            }
            done += slice; // machine.js と同じ「頼んだ分だけ進んだ」の勘定
        }
    }

    let screen = m.text_screen_string();
    // 会計が壊れていると3秒 (のつもり) が数百秒になり、駒が積み上がって
    // ゲームオーバーの "Bye! Your score:" が出る
    assert!(
        !screen.contains("Bye!"),
        "tetrisが即死した (時計が速すぎる):\n{screen}"
    );
    // 300ms周期 (SIGALRMごとに0.1ms短縮) なら3秒で10回前後。
    // 大きく外れたら時計の会計がずれている
    assert!(
        (6..=20).contains(&alarms),
        "3秒でSIGALRM {alarms}回はテンポが狂っている (期待 ~10回):\n{screen}"
    );
}

/// キーボード (8042 + IRQ1) 経由でログインし、シェルが起動する
#[test]
fn elks_accepts_keyboard_login() {
    let Some(mut m) = boot() else {
        eprintln!("images/fd1440.img が無いのでスキップ");
        return;
    };
    assert!(
        run_until(&mut m, "login:", 100_000_000),
        "loginプロンプトに到達せず"
    );

    m.devices.keyboard.type_ascii("root\n");
    // シェルのプロンプト (#) が出るまで待つ
    let ok = run_until(&mut m, "# ", 20_000_000) || {
        let s = m.text_screen_string();
        s.lines().last().map(|l| l.trim() == "#").unwrap_or(false)
    };
    let screen = m.text_screen_string();
    assert!(
        screen.contains("login: root"),
        "入力が届いていない:\n{screen}"
    );
    assert!(ok, "シェルのプロンプトが出ない:\n{screen}");
    assert!(
        m.int_counts[0x09] > 0,
        "キーボード割り込み (IRQ1) が起きていない"
    );
    assert!(m.int_counts[0x80] > 0, "システムコールが呼ばれていない");
}
