//! 走っている機械を外から覗く。
//!
//! ## なぜステップ実行ではないのか
//!
//! 実OSを動かして潰したバグを並べると、欲しかった道具の傾向がはっきりする。
//!
//! | 症状 | 本当の原因 | 効いたはずの道具 |
//! |---|---|---|
//! | 遠くで `0F 4C` のpanic | `0xA9` の幅を取り違えIPが2バイトずれた | **巻き戻し** |
//! | FreeDOSが486と誤認 | POPFDがACビットを通した | フラグの監視 |
//! | Tabでカーソルが先頭へ | BDA 0x450 を更新していなかった | **誰がこの番地を書いたか** |
//! | 画面がスクロールしない | CRTCの開始アドレスを無視 | **誰がこのポートを書いたか** |
//!
//! **一つとして「1命令ずつ進めたい」ではなかった。** panicした場所は
//! 犯行現場ではないので、順に進めても犯人には辿り着かない。
//! 要るのは「時間をさかのぼる」と「誰が触ったか」である。
//!
//! エミュレータは決定的なので、この2つが安く作れる。同じ入力なら
//! 同じ命令数で同じ状態になることは実測で確かめてある (同じ操作を2回流して
//! どちらも 779,000,000 命令ちょうどだった)。だから
//! **「n命令目まで走らせる」は完全な巻き戻しとして働く。**
//!
//! ## 速度をどう守るか
//!
//! メモリ書き込みとI/Oは最も回数の多い経路なので、分岐を増やすと直接効く。
//! 元締めの [`Debug::on`] を1つ置き、**切っている間は真偽値1つの判定で
//! 済ませる**。分岐予測が当たるので実測できる差にはならない (計測値は
//! `docs/` のベンチ節を参照)。
//!
//! 読み出しの監視は**入れていない**。[`crate::Machine::read8`] は `&self` で、
//! 記録するには可変にする必要がある。読みは書きよりさらに回数が多く、
//! 「書き込み側だけで済む仕掛けなら書き込み側に寄せる」という
//! `write8` の方針をここでも守る。I/Oは `&mut self` なので読みも監視できる。
//!
//! ## スナップショットに入れない
//!
//! デバッガの状態は**観測する側**であって機械の状態ではない。
//! 保存して戻したときにブレークポイントまで戻ると、かえって驚く。
//! 割り込み統計 (`int_counts` など) を保存していないのと同じ理由である。

use std::collections::{BTreeSet, VecDeque};

/// 止まった理由。**「どこで」ではなく「なぜ」を持つ**のが要点で、
/// ウォッチポイントは値の前後と、書いた命令の位置まで抱える
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// 実行ブレークポイント (線形アドレス)
    Break(u32),
    /// メモリ書き込み。`at` は**書いた命令の先頭**であって、書き込み時点のIPではない
    WriteMem {
        addr: u32,
        old: u8,
        new: u8,
        at: (u16, u16),
    },
    /// I/O書き込み
    WriteIo { port: u16, val: u8, at: (u16, u16) },
    /// I/O読み出し
    ReadIo { port: u16, val: u8, at: (u16, u16) },
    /// 指定した命令数に達した
    Count(u64),
}

/// 実行した命令の足跡。IPがずれる類のバグは、ずれた瞬間ではなく
/// **ずれる直前の並び**を見ないと分からない
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    pub instr: u64,
    pub cs: u16,
    pub ip: u16,
    /// 命令の先頭5バイト。逆アセンブラはまだ無いので生のまま持つ
    pub bytes: [u8; 5],
}

#[derive(Default)]
pub struct Debug {
    /// 元締め。切っている間はどのフックも真偽値1つの判定で抜ける
    pub on: bool,
    /// [`crate::Machine::step`] を呼んだ回数。**巻き戻しの座標**になるので、
    /// 機械の寿命で通し番号にする。
    ///
    /// **「実行した命令数」ではない。** HLT中も装置を進めるために step は
    /// 呼ばれ続けるので、何も実行していなくてもこの数は増える。
    /// 実行した数が要るなら [`executed`](Self::executed) を見る
    pub instr: u64,
    /// **実際にバイト列を実行した回数。**
    ///
    /// これを別に持つのは、`instr` だけを見せると嘘になるからである。
    /// ベンチのワークロードは hlt で終わるが、その後も `instr` は増え続ける。
    /// 画面に出ている数字が増えていれば人は「動いている」と読むので、
    /// **止まっていることが数字で分かる**ようにする。
    ///
    /// 2つの差がそのまま「暇にしていた時間」になる
    pub executed: u64,
    /// 実行ブレークポイント (線形アドレス CS*16+IP)
    pub code: BTreeSet<u32>,
    /// 書き込みを見張るメモリ番地
    pub mem_write: BTreeSet<u32>,
    /// 書き込みを見張るI/Oポート
    pub io_write: BTreeSet<u16>,
    /// 読み出しを見張るI/Oポート
    pub io_read: BTreeSet<u16>,
    /// ここまで来たら止まる命令数
    pub until: Option<u64>,
    /// 何も見張らずに**命令数だけ数える**。デバッガの画面を開いている間に使う。
    /// これが無いと「見張るものが無い＝元締めが切れる＝命令数が0のまま」になり、
    /// 何も壊れていないのに壊れて見える
    pub count: bool,
    /// 止まった理由。**呼び出し側が取り去るまで残る**
    pub stop: Option<Stop>,
    /// いま実行中の命令の先頭。フックが「誰が書いたか」を答えるための元
    pub at: (u16, u16),
    /// 実行の足跡。`trace_cap` が0なら何も残さない
    pub trace: VecDeque<Step>,
    pub trace_cap: usize,
    /// 一度だけブレークを見逃す番地。**止まった場所から再開できるようにする**。
    /// これが無いと同じブレークポイントで永久に止まり続ける
    resume_at: Option<u32>,
}

impl Debug {
    pub fn new() -> Self {
        Self::default()
    }

    /// 見張るものが1つでもあれば元締めを入れる。
    /// 呼び出し側が `on` を触らなくて済むよう、設定のたびに呼ぶ
    fn refresh(&mut self) {
        self.on = !self.code.is_empty()
            || !self.mem_write.is_empty()
            || !self.io_write.is_empty()
            || !self.io_read.is_empty()
            || self.until.is_some()
            || self.trace_cap > 0
            || self.count;
    }

    pub fn break_at(&mut self, lin: u32) {
        self.code.insert(lin);
        self.refresh();
    }
    pub fn watch_mem(&mut self, addr: u32) {
        self.mem_write.insert(addr);
        self.refresh();
    }
    pub fn watch_io(&mut self, port: u16, read: bool, write: bool) {
        if read {
            self.io_read.insert(port);
        }
        if write {
            self.io_write.insert(port);
        }
        self.refresh();
    }
    /// `n` 命令**先**で止める。`si` も `n命令進む` もこれ1つで書ける
    pub fn run_for(&mut self, n: u64) {
        self.until = Some(self.instr.saturating_add(n));
        self.refresh();
    }
    /// 通し番号 `n` の命令の**手前**で止める。決定的なので巻き戻しに使える
    pub fn run_to(&mut self, n: u64) {
        self.until = Some(n);
        self.refresh();
    }
    /// 命令数だけ数え始める / やめる
    pub fn set_counting(&mut self, on: bool) {
        self.count = on;
        self.refresh();
    }

    pub fn record_trace(&mut self, cap: usize) {
        self.trace_cap = cap;
        self.trace.clear();
        self.refresh();
    }
    /// 見張るものを全部外す。**命令数と足跡の設定は残す** —
    /// 「ブレークを消したら時計まで止まった」では驚く
    pub fn clear(&mut self) {
        let (cap, instr, executed, count) = (self.trace_cap, self.instr, self.executed, self.count);
        *self = Self::new();
        self.trace_cap = cap;
        self.instr = instr;
        self.executed = executed;
        self.count = count;
        self.refresh();
    }

    /// 止まった理由を受け取り、**同じ場所で再び止まらないようにする**
    pub fn take_stop(&mut self) -> Option<Stop> {
        let s = self.stop.take();
        if let Some(Stop::Break(lin)) = s {
            self.resume_at = Some(lin);
        }
        if matches!(s, Some(Stop::Count(_))) {
            self.until = None;
            self.refresh();
        }
        s
    }

    /// [`crate::Machine::step`] の入口で呼ぶ。止まるべきなら `true`。
    ///
    /// **命令数は step の呼び出し回数で数える。** 止まっている間や割り込みを
    /// 受け付けた回も1と数えるので、`boot` の例が表示する命令数と同じ座標に
    /// なる。「36,854,434 命令と出た、その少し手前に戻る」がそのまま書ける
    pub(crate) fn tick(&mut self) -> bool {
        if let Some(n) = self.until {
            if self.instr >= n {
                self.stop = Some(Stop::Count(self.instr));
                return true;
            }
        }
        self.instr += 1;
        false
    }

    /// バイト列を実行する直前に呼ぶ。止まるべきなら `true`。
    /// **実行前**に判定するので、止まった状態でその命令を見られる。
    ///
    /// 止まっている (HLT) 間は呼ばれない。呼ぶと同じ番地で永久に止まるため
    pub(crate) fn before_exec(&mut self, cs: u16, ip: u16) -> bool {
        self.at = (cs, ip);
        if self.code.is_empty() {
            return false;
        }
        let lin = (cs as u32) << 4 | ip as u32;
        if self.code.contains(&lin) {
            // 再開直後の1回だけ見逃す
            if self.resume_at == Some(lin) {
                self.resume_at = None;
                return false;
            }
            self.stop = Some(Stop::Break(lin));
            return true;
        }
        self.resume_at = None;
        false
    }
}
