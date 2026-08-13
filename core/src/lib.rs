pub mod bios;
pub mod boot;
pub mod cp437;
pub mod cpu;
pub mod debug;
pub mod dev;
pub mod disk;
pub mod mem;
pub mod snapshot;

// 移動前のパス (rustx86_core::bzimage 等) を保つ再エクスポート。
// テスト・wasm・cosim の参照はこれで壊れない
pub use boot::{bzimage, elf};
pub use mem::bus;

pub use bios::BIOS_SEG;
pub use bus::{decode_io, decode_mem, Devices, IoTarget, MemRegion};
pub use cpu::Cpu;
pub use disk::Disk;

/// 16bit機の既定RAM。8086のアドレスバスは20本なので 1MB。
/// マシンプロファイルを渡さない `Machine::new()` はこれを使う
pub const MEM_SIZE: usize = 1 << 20;

/// マシンの仕様。**いま割れるものだけ持つ** — RAMサイズ。
///
/// 装置構成 (NE2000 / virtio) はまだマシンごとに割れないので入れない
/// (取りうる値が1つの抽象は投機である)。起動方法はメソッド呼び出し
/// (`load_boot_sector` / 将来の bzImage ロード) で表せるので、これも入れない。
/// 本当に割れるのは Linux が来て RAM が 1MB に収まらなくなる、この一点だった。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineProfile {
    pub name: &'static str,
    /// RAMのバイト数。任意でよい。物理アドレスがこれを超えたら**折り返さず未マップ**
    /// (実機でRAMを超えた番地がチップセットに落ちるのと同じ)。8086の1MB折り返しは
    /// これとは別で、`cpu::lin` がリアルモードのアドレスを 0xF_FFFF に丸めている
    pub ram_bytes: usize,
    /// x87 FPU を挿しているか。
    /// 16bit機は**挿していない** — FNSTSWで書き換わらないことを見て不在を知る
    /// 検出コードがあり、気を利かせて書くと誤認させる。
    /// 32bit機は**挿している** — 現代のカーネルはFPU前提で、無いと起動しない
    pub has_fpu: bool,
    /// CPUID世代 (i586相当) を名乗るか。
    /// EFLAGSのID (bit21)・AC (bit18) が書き換え可能になり、CPUID命令が生える。
    /// 16bit機では**名乗らない** — ACを一度通しただけでFreeDOSが486と判断して
    /// CMOVを使い始めた事故がある。名乗るものを1ビット間違えるだけで、
    /// 相手は別の道を歩き出す
    pub has_cpuid: bool,
}

impl MachineProfile {
    /// 16bit機 (ELKS / FreeDOS)。1MB
    pub const PC_16BIT: Self = Self {
        name: "16bit PC",
        ram_bytes: MEM_SIZE,
        has_fpu: false,
        has_cpuid: false,
    };

    /// 32bit PC (Linux用)。`mb` MB。任意のMB数を取れる (物理は折り返さず、
    /// RAMを超えた番地は未マップになるので2の冪でなくてよい)
    pub fn pc_32bit(mb: usize) -> Self {
        Self {
            name: "32bit PC",
            ram_bytes: mb << 20,
            has_fpu: true,
            has_cpuid: true,
        }
    }
}

/// 何命令ごとに装置を進めるか。
///
/// 本来はCPUのクロックと装置のクロックを別々に数えるべきだが、このエミュレータは
/// サイクル数を持っていない。「1命令 ≒ 一定時間」と割り切って、まとめて進める。
///
/// ELKSやLinuxが要求するのは**周期割り込みが一定の間隔で来ること**であって、
/// 実時間との一致ではない。時計がずれても動作は壊れない。
pub const INSTRUCTIONS_PER_TICK: u32 = 64;

/// 1回の tick で装置に渡すクロック数。
///
/// 実測 (Tier 1c 時点) でおよそ 1億命令/秒 出ているので、
/// 1命令 ≒ 10ns → 64命令 ≒ 640ns。PITの入力は 1.193182 MHz (≒838ns周期) なので、
/// 64命令あたり 1クロックに近い。実時間に大きくは外れない
pub const PIT_CLOCKS_PER_TICK: u32 = 1;

/// IRQ0 (PIT) の割り込み線
pub const IRQ_TIMER: u8 = 0;
/// IRQ1 (キーボード) の割り込み線
pub const IRQ_KEYBOARD: u8 = 1;
/// IRQ4 (COM1) の割り込み線
pub const IRQ_COM1: u8 = 4;
/// NE2000の定番IRQ。DOSのパケットドライバの既定値に合わせる
pub const IRQ_NET: u8 = 3;

/// マシン全体。メモリとBIOS HLE (高位エミュレーション) を持つ。
/// 本物のBIOSは実装せず、INT命令をフックして最小限のサービスだけ提供する。
pub struct Machine {
    pub cpu: Cpu,
    pub mem: Vec<u8>,
    /// 保留中のハードウェア割り込みベクタ。IFが立っている命令境界で受け付ける。
    /// Tier 2a で 8259 PIC がここへ挙手する
    pub pending_irq: Option<u8>,
    /// PICに未処理の要求がある印。ベクタはまだ決めない (INTAで決まる)
    pic_service: bool,
    /// いま「CPU自身の内部アクセス」中か — 記述子表 (GDT/IDT/TSS) の読みと、
    /// 例外配送のスタック操作は、CPL=3でも**スーパーバイザ権限で行われる**
    /// (実機の暗黙のシステムアクセス)。この間はページのU/S検査を免除する。
    /// これを忘れると、リング3からの例外配送がIDT読みで弾かれてゲートがゴミになる
    pub sys_access: std::cell::Cell<bool>,
    /// 命令の実行中に起きたページフォールト。命令の終わりで #PF として配送する。
    /// Cell なのは読み経路 (&self) からも失敗を記録するため。
    /// 実機は命令の途中で中断するが、うちは**完走させてから巻き戻す**
    /// (フォールトした書き込みは捨てているので、再実行しても二重にならない)
    pub pending_fault: std::cell::Cell<Option<PageFault>>,
    /// I/Oポート空間にぶら下がる装置。中身は Tier 2b で実装する
    pub devices: Devices,
    /// 誰も名乗り出なかったポート番号。
    ///
    /// 実機は未接続のポートを読むと 0xFF が返るだけで、OSはこれを使って
    /// 装置の有無を探る。だから panic はできない。かといって黙って捨てると
    /// 「なぜ動かないのか」の手がかりが消えるので、触られた番号だけ覚えておく
    pub unhandled_io: std::collections::BTreeSet<u16>,
    /// テキストVRAMに書き込みがあったか。描画側が読んだら下ろす
    pub vram_dirty: bool,
    /// `0x66` (オペランドサイズ) を付けて実行されたオペコード。
    ///
    /// **幅対応を忘れた命令は静かに壊れる。** 即値や退避の長さが変わるので、
    /// 16bitのまま実行するとIPがずれ、以後はデータを命令として食い始める。
    /// panicも出ないまま遠くで暴走するので、**来たものを控えておく**。
    pub prefixed_ops: std::collections::BTreeSet<u8>,
    /// `prefixed_ops` の「もう控えたか」を配列で持つ。
    /// ホットパス (毎プレフィクス命令) で BTreeSet を歩かないため
    pub prefixed_seen: [bool; 256],
    /// ユーザー空間で #UD にした未実装命令の理由 (観測用)。
    /// 機械は止めない — OSがSIGILLで裁く。実装すべきものの一覧になる
    pub ud_user: std::collections::BTreeSet<String>,
    /// ゲストが設定しようとしたビデオモード。
    ///
    /// **テキスト以外は黙って無視している**ので、グラフィックスを要求された
    /// ことに気づけない。画面が真っ白なのが「何も描いていない」のか
    /// 「描いた先が無い」のかを区別するために控えておく
    pub video_modes: std::collections::BTreeSet<u8>,
    /// 装置を進めるまでの残り命令数。
    ///
    /// 装置を毎命令進めると、最も回数の多い経路に仕事が乗る。
    /// カウントダウン1本にしておけば、ほとんどの命令は「1減らして分岐」だけで済む
    tick_countdown: u32,
    /// INT 10h テレタイプ出力の蓄積 (画面代わり)
    pub console: Vec<u8>,
    /// ブートしたディスク。INT 13h のHLEが読む
    pub disk: Option<Disk>,
    /// 最初に起きたCPU例外の (ベクタ番号, CS, IP)。
    /// 実OSを動かすと「どこで壊れたか」だけが手がかりになるので控えておく
    pub first_fault: Option<(u8, u16, u32)>,
    /// ベクタごとの発生回数。全部数える (周期割り込みで溢れないよう回数だけ)
    pub int_counts: Vec<u32>,
    /// ベクタごとの初出位置 (CS, IP)
    pub int_first: Vec<(u16, u32)>,
    /// 直近の割り込み (ベクタ, CS, IP)。**panic直前に何が起きたかはここに出る**
    pub int_recent: std::collections::VecDeque<(u8, u16, u32)>,
    /// このマシンの仕様 (RAMサイズなど)。覗き窓に出す
    pub profile: MachineProfile,
    /// 外から覗くための仕掛け。**機械の状態ではない**のでスナップショットには入れない
    pub dbg: debug::Debug,
    /// 未実装にぶつかって止まった理由。
    ///
    /// **panicではなくこれで止める**のが要点。panicするとwasmインスタンスが
    /// 死んで、その瞬間のレジスタもスタックも覗けなくなる。Linux起動は
    /// 「走らせる→止まった所を見る→実装する」の繰り返しなので、
    /// **止まった所が生きたまま見える**ことが道具の生命線になる。
    /// 名前は報告する (静かに壊れない方針は維持) が、機械は殺さない
    /// トラップの発生地点を控えるための「実行前IP」。毎命令 step 入口で更新
    pub trap_ip: u32,
    pub trap: Option<Trap>,
    pub halted: bool,
    /// BIOS HLE (CS==0xF000 をホスト関数で肩代わり) を使うか。
    /// 生ROM実行 (test386などのテストROM) では 0xF000 が**実行すべき実コード**
    /// なので、`boot_rom` がこれを下ろして素通しにする。
    /// スナップショットには入れない (ROM実行は使い切りのテスト走行)
    pub bios_hle: bool,
    /// HLEのINT 08hがINT 1Ch (利用者タイマフック) をゲストへ配送中の印。
    /// 1Chから戻ってきた2周目のINT 08hは締め (IRET) だけを行う。
    /// スナップショットには入れない — 復元がチェーンの最中に当たっても、
    /// 起きるのは「ティックが1つ余分に進む」だけで自然に回復する
    tick_chain: bool,
    /// POST診断ポート (0x190) に書かれた進行コードの足跡。
    /// テストROM (test386) がテスト番号を書く — どこまで進んで死んだかの証跡
    pub post_trail: Vec<u8>,
    /// デコード済み命令キャッシュ (ADR-0007 P1a)。中身は cpu::dcache
    pub dcache: cpu::dcache::DecodeCache,
    /// フォールト巻き戻し用の命令前CPU控え (常設の器 + この命令で控えたかの印)。
    /// **実行する側が「要るときだけ」控える**: フォールバック経路は従来どおり毎回、
    /// キャッシュ済みuopは「メモリに触るものだけ」— レジスタ間演算・jcc・lea等は
    /// #PFが起き得ないので複写ごと省く。機械の状態ではないのでスナップショット外
    pub(crate) fault_save: Cpu,
    /// 薄い控えの器 (キャッシュ済みuop用)。どちらの控えが有効かは kind が語る
    fault_slim: cpu::SlimSave,
    fault_save_kind: FaultSaveKind,
    /// アイドル (HLT) の早送りが飛ばした仮想命令数の累計。
    ///
    /// **走らせる側が実時間との釣り合いを取るための読み値**で、機械の状態では
    /// ない (スナップショットに入れない)。ランナーはスライスごとにこれを
    /// 読み取って、「飛ばした時間ぶんだけ実時間で待つ」ことでゲストの時計を
    /// 実時間に繋ぎ止める。忙しい実行は自由に速く、暇は実時間どおりに流れる
    pub idle_skipped: u64,
    /// オペコードの実行回数 (計測用)。0..256 = 1バイト命令、256.. = 0F 2バイト目。
    /// 数えるのは opstats フィーチャを立てたときだけ (通常ビルドではコストゼロ)。
    /// **どの命令をデコードキャッシュに入れるかはこの実測で決める** (推測しない)
    pub op_counts: Vec<u64>,
    /// TLB — 線形→物理の変換の写し。**ページングの最大のボトルネックを消す。**
    ///
    /// ページング有効時、変換1回は2段の表 (PDE→PTE) を読む = 物理メモリ2回。
    /// これを毎バイトやると、4バイト読むのに変換4回×表2回=8回の余計な読み。
    /// 実CPUと同じく、一度歩いた結果を控えて次から表を引かない。
    /// 決定的なので写しても結果は同じ — 無効化は mov cr3 / invlpg / cr0 で行う。
    /// `Cell` なのは読み経路 (&self) からも埋めるため
    tlb: Vec<std::cell::Cell<TlbEntry>>,

    /// ページウォークが立てるべき A/D ビットの持ち越し (葉/PDEの物理番地, ORする値)。
    /// 歩く経路は読みと同じ `&self` なので mem に直接書けない —
    /// **次の命令境界 (&mut) で反映する**。ゲストが「触った直後の同一命令内」で
    /// 表を読み返す手段は無いので、この1命令の遅延は観測できない
    ad_queue: std::cell::RefCell<Vec<(u32, u8)>>,
    /// ad_queue が空でない印。毎命令見るのはこの真偽値1つ
    ad_pending: std::cell::Cell<bool>,

    /// データアクセスのセグメント検査 (limit/書込可否) が見つけた違反。
    /// 検査は読み経路 (&self) からも走るので #PF と同じ「控えて命令境界で配送」。
    /// 値は例外ベクタ (13=#GP / 12=#SS)。エラーコードは常に0
    pending_seg_fault: std::cell::Cell<Option<u8>>,
}

/// TLBの1エントリ。present な変換だけを載せる (不在フォールトは載せない)。
/// 権限 (書ける/ユーザーで触れる) はここに持ち、CPLとWPは引くたびに新しく見る
/// repr(C)でフィールドの並びを安定させておく (外部ビューとの契約の名残 —
/// 並び替えはレイアウト差の測定ノイズにもなるので触らない)
#[repr(C)]
#[derive(Clone, Copy)]
struct TlbEntry {
    /// 仮想ページ番号 (la >> 12)。`INVALID` は空きスロット
    tag: u32,
    /// 物理ページの4K境界の先頭。**下位12bitは空くので旗を詰める** (S3 —
    /// boolで持つと4096スロットの器が12→16バイトに肥え、L1を余計に食う):
    ///   bit0 = writable (PDEとPTEのR/Wが両方立っている)
    ///   bit1 = user_ok (PDEとPTEのU/Sが両方立っている)
    ///   bit2 = dirty (Dビットを立てた後か — falseのうちに書き込みが来たら
    ///          葉へDを書く。実CPUのTLBも「Dを立てたか」を控えて二度書きしない)
    base_flags: u32,
    /// 葉エントリ (PTE、4MBページならPDE) の物理番地。Dビットを立てる宛先
    leaf: u32,
}

/// フォールト巻き戻しの控えの種類。
/// Slim はキャッシュ済みuop用 (書き得る部分だけ)、Full はフォールバック経路用
#[derive(Clone, Copy, PartialEq, Eq)]
enum FaultSaveKind {
    None,
    Full,
    Slim,
}

const TLB_INVALID: u32 = 0xFFFF_FFFF;
/// TLBのスロット数 (直接マップ)。4096で16MB分のホットページを覆える
const TLB_SLOTS: usize = 4096;

/// ページ変換の失敗。#PF として配送される
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageFault {
    /// 失敗した線形アドレス (CR2 へ入る)
    pub la: u32,
    /// 書き込みだったか (エラーコード bit1)
    pub write: bool,
    /// ページは居たが保護で弾いたか (エラーコード bit0)
    pub present: bool,
}

/// 実行を止めた「未実装」の中身。どの命令が、どこで、を抱える
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trap {
    pub reason: String,
    pub cs: u16,
    pub ip: u32,
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

impl Machine {
    /// 既定の 16bit機 (1MB)。既存の呼び出し元はこれで従来どおり
    pub fn new() -> Self {
        Self::with_profile(MachineProfile::PC_16BIT)
    }

    /// 仕様を指定して作る。RAMサイズがプロファイルで決まる
    pub fn with_profile(profile: MachineProfile) -> Self {
        Self {
            cpu: Cpu::new(),
            mem: vec![0; profile.ram_bytes],
            pending_irq: None,
            pic_service: false,
            pending_fault: std::cell::Cell::new(None),
            sys_access: std::cell::Cell::new(false),
            devices: Devices::new(),
            unhandled_io: std::collections::BTreeSet::new(),
            vram_dirty: false,
            prefixed_ops: std::collections::BTreeSet::new(),
            prefixed_seen: [false; 256],
            video_modes: std::collections::BTreeSet::new(),
            ud_user: std::collections::BTreeSet::new(),
            tick_countdown: INSTRUCTIONS_PER_TICK,
            dcache: cpu::dcache::DecodeCache::new(profile.ram_bytes),
            op_counts: vec![0; 512],
            console: Vec::new(),
            disk: None,
            first_fault: None,
            int_counts: vec![0; 256],
            int_first: vec![(0, 0); 256],
            int_recent: std::collections::VecDeque::with_capacity(33),
            profile,
            dbg: debug::Debug::new(),
            trap_ip: 0,
            trap: None,
            halted: false,
            bios_hle: true,
            tick_chain: false,
            post_trail: Vec::new(),
            fault_save: Cpu::new(),
            fault_slim: cpu::SlimSave::default(),
            fault_save_kind: FaultSaveKind::None,
            idle_skipped: 0,
            tlb: (0..TLB_SLOTS)
                .map(|_| {
                    std::cell::Cell::new(TlbEntry {
                        tag: TLB_INVALID,
                        base_flags: 0,
                        leaf: 0,
                    })
                })
                .collect(),
            ad_queue: std::cell::RefCell::new(Vec::new()),
            ad_pending: std::cell::Cell::new(false),
            pending_seg_fault: std::cell::Cell::new(None),
        }
    }

    /// データアクセスの実効番地 (**セグメント検査つき**)。
    ///
    /// 保護モードの約束: limitの外・読み取り専用への書き込み・ヌルセグメントの
    /// 使用は、番地に化ける前に例外になる (**セグメンテーションはページングより
    /// 先**)。違反は #PF と同じく控えて命令境界で配送し、ここでは**毒番地**
    /// (RAM外) を返してアクセスを空振りさせる — 巻き戻しがどのみち全部捨てる。
    /// リアルモードとV86は無検査 (64Kの折り返しはオフセットの幅が既に守っている)
    #[inline]
    pub(crate) fn data_addr(&self, seg: usize, off: u32, size: u32, write: bool) -> u32 {
        if !self.cpu.pe() || self.cpu.vm86() {
            return self.cpu.lin(seg, off);
        }
        let h = self.cpu.hidden[seg];
        // S1: 平坦セグメント (Linuxの常態) は検査が恒真で base加算も恒等 —
        // 1分岐で素通し。ここが毎命令の互換税+10%の大半だった (perf-log 2026-08-12)
        if h.flat_rw() {
            return off;
        }
        let vector = if seg == cpu::SS { 12 } else { 13 };
        // ヌル (または不在) セグメントは使った瞬間に咎める
        let ok = if h.access & 0x80 == 0 {
            false
        } else if write && (h.access & 0x08 != 0 || h.access & 0x02 == 0) {
            // コードセグメントには書けない。データは W ビットが要る
            false
        } else if !write && h.access & 0x08 != 0 && h.access & 0x02 == 0 {
            // 実行専用コードは読めもしない
            false
        } else if h.access & 0x1C == 0x14 {
            // 伸長方向が逆 (expand-down データ、スタック用):
            // 有効なのは limit より**上** から器の天井まで
            let top: u64 = if h.big { 0xFFFF_FFFF } else { 0xFFFF };
            off > h.limit && (off as u64 + size as u64 - 1) <= top
        } else {
            (off as u64 + size as u64 - 1) <= h.limit as u64
        };
        if ok {
            self.cpu.lin(seg, off)
        } else {
            if self.pending_seg_fault.get().is_none() {
                self.pending_seg_fault.set(Some(vector));
            }
            0xFFFF_FFFF
        }
    }

    /// ページウォークからの A/D ビット持ち越し (&self経路用)。反映は [`Self::flush_ad`]
    pub(crate) fn queue_ad(&self, pa: u32, mask: u8) {
        self.ad_queue.borrow_mut().push((pa, mask));
        self.ad_pending.set(true);
    }

    /// 持ち越した A/D ビットを物理メモリの表へ反映する。命令境界で呼ぶ。
    /// **毎命令の支払いは真偽値1つ** — 空チェックをインラインに残し、
    /// 実仕事 (稀) だけ関数呼び出しにする (S2。呼び出しごと払うと
    /// ホットループに call が1本増える)
    #[inline]
    pub(crate) fn flush_ad(&mut self) {
        if self.ad_pending.get() {
            self.flush_ad_slow();
        }
    }

    /// ORなので二重反映は無害。表がRAM外を指していたら黙って捨てる
    /// (壊れた表で歩いた結果 — フォールト側が別途裁いている)
    #[cold]
    fn flush_ad_slow(&mut self) {
        self.ad_pending.set(false);
        let mut q = self.ad_queue.borrow_mut();
        for &(pa, mask) in q.iter() {
            if let Some(b) = self.mem.get_mut(pa as usize) {
                *b |= mask;
            }
        }
        q.clear();
    }

    /// 装置を進め、挙手があればPICへ渡す。**一周はこうなっている**:
    /// PITがカウンタ0を下ろしきってIRQ0を出す → PICが優先順位を見て受理し、
    /// ICW2で設定されたベースから割り込みベクタを決める → CPUが命令境界で受け取る。
    /// この経路のどこか1つでも欠けるとOSのスケジューラが動かない。
    /// 装置を `ticks` 回ぶん進める。通常経路は1、アイドルの早送りだけが
    /// まとめて渡す。クロックはまとめても1回ずつでも同じ数だけ進む
    fn tick_devices(&mut self, ticks: u32) {
        if self.devices.pit.tick(ticks * PIT_CLOCKS_PER_TICK) > 0 {
            self.devices.pic[0].raise(IRQ_TIMER);
        }
        // 時計もPITと同じクロックで進める。**ここで進めるのが要点**で、
        // INT 08h の中で進めるとOSが自前のハンドラを入れた瞬間に時計が止まる
        self.devices.cmos.tick(ticks * PIT_CLOCKS_PER_TICK);
        if self.devices.uart.irq_pending {
            self.devices.pic[0].raise(IRQ_COM1);
        }
        if let Some(net) = &self.devices.net {
            if net.irq_pending() {
                self.devices.pic[0].raise(IRQ_NET);
            }
        }
        // キーボードは割り込み駆動。**1バイトにつき1回だけ**挙手する
        if self.devices.keyboard.take_irq() {
            self.devices.pic[0].raise(IRQ_KEYBOARD);
        }
        // **ここでは acknowledge しない。** ベクタ番号が決まるのは CPU が
        // INTA で受ける瞬間で、それより早くベクタを固定すると、OSがPICを
        // 再マップした後に**古いベクタ**が飛び出す (Linuxの sti 直後に
        // BIOS時代の vector 8 = #DF タスクゲートへ突っ込んで実際に死んだ)
        self.pic_service = self.devices.pic[0].has_pending();
    }

    /// PICに未処理の要求があるか (CPUが受けにいくべきか)
    fn pic_has_service(&self) -> bool {
        self.pic_service
    }

    /// フォールトに備えてCPUを控える (要るときだけ呼ばれる)。
    /// 控えの使い道は #PF・セグメント例外 (#GP/#SS) の巻き戻しと CPL=3 の
    /// #UD 巻き戻し — どれも保護モードでしか起きないので、条件は PE ひとつ。
    /// 控えが要る状況か。使い道は #PF の巻き戻し (ページング有効時しか
    /// 起きない) と、ユーザー空間 (CPL=3、v86含む) の #UD 巻き戻しの2つ —
    /// どちらも起き得ない「ページングOFFかつリング0」(bzImage解凍ステブ等)
    /// では複写が純粋な無駄 (step_innerの注釈の条件を、ゲートにも正確に写す。
    /// 従来はpe()で粗く見ていた = 解凍ステブ540Mで払い続けていた)
    #[inline]
    fn guard_needed(&self) -> bool {
        self.cpu.pe() && (self.cpu.pg() || self.cpu.cpl() == 3 || self.cpu.vm86())
    }

    /// Boxの器は使い回して確保を避ける
    #[inline]
    pub(crate) fn guard_save(&mut self) {
        if self.guard_needed() {
            self.fault_save = self.cpu.clone();
            self.fault_save_kind = FaultSaveKind::Full;
        }
    }

    /// [`guard_save`](Self::guard_save) の薄い版 — キャッシュ済みuop用。
    /// uopが書き得るのは regs/ip/フラグだけなので、そこだけ控える (~76B)。
    /// Cpu丸ごと (~400B) の複写はプロファイルの memmove 11%だった
    #[inline]
    pub(crate) fn guard_save_slim(&mut self) {
        if self.guard_needed() {
            self.cpu.save_slim(&mut self.fault_slim);
            self.fault_save_kind = FaultSaveKind::Slim;
        }
    }

    /// guard_save_slim の「巻き戻し先ip指定」版 — exec内 (advance_ip後) 用
    pub(crate) fn guard_save_slim_at(&mut self, ip: u32) {
        if self.guard_needed() {
            self.cpu.save_slim_at(&mut self.fault_slim, ip);
            self.fault_save_kind = FaultSaveKind::Slim;
        }
    }

    /// 控えからCPUを命令前の姿へ戻す。控えが無ければ false
    fn guard_restore(&mut self) -> bool {
        match self.fault_save_kind {
            FaultSaveKind::None => false,
            FaultSaveKind::Full => {
                self.cpu = self.fault_save.clone();
                true
            }
            FaultSaveKind::Slim => {
                self.cpu.restore_slim(&self.fault_slim);
                true
            }
        }
    }

    /// アイドル (HLT) の早送り。
    ///
    /// HLT中のCPUは命令を実行しない。次に何かが起きるとしたら装置イベント
    /// だけなので、1命令ぶんずつ空回りせず、**次のPITパルスまで時計と装置を
    /// 一気に進める**。シェルのプロンプトで待っているだけの機械が、
    /// ホストのCPUを食いつぶすのをやめる。
    ///
    /// 約束は一つ: **飛ばした分だけTSCと装置のクロックも進める。**
    /// 止まるのはCPUであって時計ではない。時計を置き去りにすると、
    /// nanosleep したプロセスが永遠に起きない罠 (Tier 3bで実際に踏んだ) を
    /// 逆向きにもう一度踏むことになる。
    ///
    /// もう一つの約束: **予算 (budget) を超えて飛ばさない。**
    /// run系は「予算=仮想時間」で回っているのに、ここが1回で次のPITパルスまで
    /// (100Hzなら10ms ≒ 763k命令分) 飛ぶと、予算6千の run_slice が127倍
    /// 超過する。呼ぶ側は「頼んだ分だけ進んだ」と勘定するので、アイドル中の
    /// ゲストの時計だけが実時間の百倍で流れた — ELKSのtetrisは read(2) を
    /// SIGALRM (300ms) で切ってテンポを作るゲームで、駒が一瞬で積み上がって
    /// 即ゲームオーバーになった (実際になった)。予算の途中までしか飛ばず、
    /// 残りは次の呼び出しが続きから飛ぶ。
    fn idle_fast_forward(&mut self, budget: u64) {
        // 既に挙手があるなら飛ばさない。IF=1なら次のstepで配送されて起きるし、
        // IF=0で寝ている機械の時計だけが暴走するのも防ぐ (従来の1刻みに落とす)
        if self.pending_irq.is_some() || self.pic_service {
            return;
        }
        // タイマが止まっているなら、起こせるのは外部入力 (キー・シリアル) だけ。
        // それはスライスの外から来るので、run() 側の「タイマも止まっていれば
        // 抜ける」判定に任せ、ここでは飛ばさない
        let Some(clocks) = self.devices.pit.clocks_until_irq0() else {
            return;
        };
        // クロック → tick数 (切り上げ)。パルスが出る tick まで飛ぶ
        let to_irq = clocks.div_ceil(PIT_CLOCKS_PER_TICK).max(1) as u64;
        // 予算内に収まる tick 数。最低1 tick は進める — ゼロだと進捗が無く
        // run系のループが空回りする (超過は高々 tick_countdown ≦ 64命令分)
        let affordable = if budget <= self.tick_countdown as u64 {
            1
        } else {
            1 + (budget - self.tick_countdown as u64) / INSTRUCTIONS_PER_TICK as u64
        };
        // 予算が先に尽きるならパルスの手前で止まる。IRQは出ないので機械は
        // 寝たままだが、それでよい — 次の呼び出しが続きから飛ぶ
        let ticks = to_irq.min(affordable) as u32;
        // 命令数に換算: 次のtickまでの残り + そこから先のtick分。
        // この間 step() は呼ばれなかったことになるので、TSCをまとめて進める
        let skip = self.tick_countdown as u64 + (ticks as u64 - 1) * INSTRUCTIONS_PER_TICK as u64;
        self.cpu.tsc = self.cpu.tsc.wrapping_add(skip);
        self.idle_skipped = self.idle_skipped.wrapping_add(skip);
        self.tick_countdown = INSTRUCTIONS_PER_TICK;
        // 装置をまとめて進める。PITはここで丁度1パルス出し、次のstepの
        // 冒頭で割り込みとして配送されて目が覚める
        self.tick_devices(ticks);
    }

    /// 未実装にぶつかった。**その命令を実行する前の CS:IP** で止める。
    /// 呼び出し側は直後に return して、命令を実行しないこと
    pub(crate) fn trap(&mut self, reason: String) {
        self.trap = Some(Trap {
            reason,
            cs: self.cpu.sregs[cpu::CS],
            ip: self.trap_ip,
        });
    }

    /// BIOSサービスが返した成否をフラグとして呼び出し元へ届ける。
    ///
    /// **`IRET` はスタックに積まれたFLAGSで上書きしてしまう。** サービスが
    /// `CF` や `ZF` を立てても、そのまま戻ると消える。実BIOSも同じ事情を
    /// 抱えていて、**積まれている方のFLAGSを書き換えてから**戻る。
    ///
    /// x86のBIOSが慣例として「成否はキャリーフラグで返す」形なのに、
    /// この一手間が要るのは面白いところである。
    fn return_flags_to_caller(&mut self) {
        // スタック: [SP]=IP [SP+2]=CS [SP+4]=FLAGS
        let sp = self.cpu.regs[cpu::SP] as u16;
        let addr = cpu::operand::linear(self.cpu.sregs[cpu::SS], sp.wrapping_add(4));
        let stacked = self.read16(addr);
        let keep = (cpu::CF | cpu::ZF) as u16;
        self.write16(addr, (stacked & !keep) | (self.cpu.eflags() as u16 & keep));
    }

    /// カーソルの位置 (行, 桁)。CRTCが持っている
    pub fn cursor_pos(&self) -> (usize, usize) {
        let off = self.devices.crtc.cursor_offset() as usize;
        (off / bus::TEXT_COLS, off % bus::TEXT_COLS)
    }

    /// カーソルを (行, 桁) へ動かす。
    ///
    /// **CRTCとBIOSデータエリアの両方に書く。** 実機でもこの2箇所に同じ位置が
    /// 載っていて、画面はCRTCを見るが、**ソフトはBDAの方を直接読むことがある**。
    /// BDA側 (0x450、ページ0のカーソル位置) を更新していなかったので、
    /// 覗いた側からはいつまでも「行0桁0」に見えていた。
    pub fn set_cursor_pos(&mut self, row: usize, col: usize) {
        let row = row.min(bus::TEXT_ROWS - 1);
        let col = col.min(bus::TEXT_COLS - 1);
        self.devices
            .crtc
            .set_cursor_offset((row * bus::TEXT_COLS + col) as u16);
        self.write16(0x450, (row as u16) << 8 | col as u16);
    }

    /// 描画側が読んだ印。次の書き込みまで dirty が下りる
    pub fn take_vram_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.vram_dirty, false)
    }

    /// NE2000を挿す。呼ばなければ機械にNICは無く、起動はビット同一のまま
    /// (ADR-0017の不変条件)。macはゲストのDHCP/ARPでそのまま名乗られる
    pub fn net_attach(&mut self, mac: [u8; 6]) {
        self.devices.net = Some(dev::Ne2000::new(mac));
    }

    /// 外の世界 (WebSocket等) から届いたEthernetフレームを受信リングへ。
    /// シリアルの feed と同じ境界 — 入れるタイミングは外側が決め、
    /// 同じ列を同じタイミングで入れれば実行は決定的になる
    pub fn net_inject_frame(&mut self, frame: &[u8]) -> bool {
        match &mut self.devices.net {
            Some(net) => net.inject_frame(frame),
            None => false,
        }
    }

    /// ゲストが送信したフレームを回収する (読むと消える)。serial_outと同じ作法
    pub fn net_take_frames(&mut self) -> Vec<Vec<u8>> {
        match &mut self.devices.net {
            Some(net) => net.tx_out.drain(..).collect(),
            None => Vec::new(),
        }
    }

    /// PCスピーカーが今出している音の周波数 (Hz)。無音なら None。
    ///
    /// 実機の配線そのまま: PIT カウンタ2の矩形波が、システム制御ポート
    /// 0x61 の bit0 (ゲート) と bit1 (スピーカーへの通電) の**両方が立って
    /// いるときだけ**スピーカーに届く。BIOSビープもDOSのゲームもこの2bitを
    /// 立ててから分周値を書く。
    ///
    /// イベントやコールバックにはしない — 画面 (take_vram_dirty) と同じく
    /// **描画側がスライス境界でポーリングする**流儀。coreは状態を返すだけで
    /// 壁時計もオーディオAPIも知らないので、決定性に影響しない
    pub fn speaker_tone(&self) -> Option<f64> {
        if self.devices.sysctl & 0b11 != 0b11 {
            return None;
        }
        self.devices.pit.speaker_freq()
    }

    /// テキスト画面を文字列にする (属性を捨てて文字コードだけ拾う)。
    /// テストと確認用で、実際の描画は色も使う
    pub fn text_screen_string(&self) -> String {
        let v = self.text_vram();
        (0..bus::TEXT_ROWS)
            .map(|row| {
                let line: String = (0..bus::TEXT_COLS)
                    .map(|col| cp437::to_char(v[(row * bus::TEXT_COLS + col) * bus::TEXT_CELL]))
                    .collect();
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_string()
    }

    /// 16bitのI/Oは連続する2ポートへのアクセスとして扱う
    pub fn io_read16(&mut self, port: u16) -> u16 {
        self.io_read8(port) as u16 | (self.io_read8(port.wrapping_add(1)) as u16) << 8
    }

    pub fn io_write16(&mut self, port: u16, val: u16) {
        self.io_write8(port, val as u8);
        self.io_write8(port.wrapping_add(1), (val >> 8) as u8);
    }

    /// 32bitのI/O。専用装置 (PCIコンフィグ等) を積むまでは16bit×2で表す
    pub fn io_read32(&mut self, port: u16) -> u32 {
        self.io_read16(port) as u32 | (self.io_read16(port.wrapping_add(2)) as u32) << 16
    }

    pub fn io_write32(&mut self, port: u16, val: u32) {
        self.io_write16(port, val as u16);
        self.io_write16(port.wrapping_add(2), (val >> 16) as u16);
    }

    /// 1サイクル進める。
    ///
    /// 順序に意味がある。**割り込みの受付は命令の途中ではなく境界で行う**。
    /// 命令の実行中に割り込むと、書き換え途中のレジスタやスタックのまま
    /// ハンドラへ飛ぶことになり、`IRET` で戻っても再開できない。
    /// 1命令進める。アイドル早送りは無制限 (次のPITパルスまで一気に飛ぶ)。
    /// 予算の中で回すときは [`Self::step_budgeted`] を使う — run系が
    /// 「予算=仮想時間」の約束を守るのはそちら経由である
    #[inline]
    pub fn step(&mut self) {
        // 連結は0 — 「1命令進める」の契約を守る (デバッガ・cosimが頼る粒度)
        self.step_inner(u64::MAX, 0);
    }

    /// 1命令**以上**進める。HLT中の早送りは `idle_budget` (仮想時間の残り予算)
    /// までに制限する — 予算を超えて時計が飛ぶと、呼ぶ側の時間の勘定が壊れる。
    /// 予算の残りはブロック連結 (B4) の連結許可量も兼ねる — 進んだ量は
    /// 常にTSCに現れるので、run系の「予算=仮想時間」の約束はそのまま保たれる
    pub fn step_budgeted(&mut self, idle_budget: u64) {
        self.step_inner(idle_budget, idle_budget.saturating_sub(1));
    }

    /// 実体。`chain_extra` = 最初の1命令に**追加して**連結実行してよい命令数
    fn step_inner(&mut self, idle_budget: u64, chain_extra: u64) {
        // 未実装で止まっていたら、以後は何もしない (run系がここで抜ける)
        if self.trap.is_some() {
            return;
        }
        // 前の命令のページウォークが立てた A/D ビットを表へ反映
        // (歩く経路は &self なので、ここ (&mut) まで持ち越されている)
        self.flush_ad();
        // 0. デバッガ。切っていれば真偽値1つで抜ける。
        //    命令数は**この呼び出しの回数**で数えるので、`boot` の例が出す
        //    命令数と同じ座標になる (決定的なので巻き戻しの目盛りになる)
        if self.dbg.on && self.dbg.tick() {
            return;
        }
        // 1. 保留中のハードウェア割り込みを受け付ける (IFが立っているときだけ)。
        //    HLTで止まっていてもここで目を覚ます — 割り込み待ちのHLTが
        //    成立するのはこのため
        //    `is_some()` を先に見るのは、`take()` が None を書き戻すため。
        //    毎命令の書き込みになり、実測で数%効いた (ベンチが捕まえた)
        if self.cpu.flag(cpu::IF) {
            if self.pending_irq.is_some() {
                let vec = self.pending_irq.take().unwrap();
                self.halted = false;
                cpu::interrupt(self, vec);
                return;
            }
            // PICからは**受ける瞬間に**ベクタをもらう (INTA相当)
            if self.pic_has_service() {
                if let Some(vec) = self.devices.pic[0].acknowledge() {
                    self.pic_service = self.devices.pic[0].has_pending();
                    self.halted = false;
                    cpu::interrupt(self, vec);
                    return;
                }
                self.pic_service = false;
            }
        }
        // 2. 時計を進める。**HLT中も進める**のが要点。
        //
        // TSCを「実行した命令数」にしていたため、アイドル (HLT) の間だけ
        // ゲストの時間が止まっていた。カーネルは較正した周波数から
        // 「TSCがNカウント進んだら120ms」と数えるので、nanosleep で寝た
        // プロセスは**二度と起きなかった** (snakeの蛇が1歩も動かなかった)。
        // 実機のTSCはHLT中もクロックで進む — 止まるのはCPUであって時計ではない
        self.cpu.tsc = self.cpu.tsc.wrapping_add(1);

        // 装置を進める。毎命令ではなくまとめて進め、ホットパスの負担を抑える
        self.tick_countdown -= 1;
        if self.tick_countdown == 0 {
            self.tick_countdown = INSTRUCTIONS_PER_TICK;
            self.tick_devices(1);
        }

        if self.halted {
            self.idle_fast_forward(idle_budget);
            return;
        }

        // 2.5 実行の直前。**バイト列を実行する前**に判定するので、止まった
        //     状態でその命令そのものを見られる。止まっている間は通らない
        //     (通すと同じ番地で永久に止まる)
        if self.dbg.on {
            let (cs, ip) = (self.cpu.sregs[cpu::CS], self.cpu.ip);
            // 線形アドレスは隠しレジスタ経由。sel<<4 は保護モードで嘘になる
            let lin = self.cpu.lin(cpu::CS, ip);
            if self.dbg.before_exec(lin, cs, ip) {
                self.dbg.instr -= 1; // 実行しなかったので数え戻す
                return;
            }
            // ここを通ったものだけが**本当に実行される**。HLT中は通らないので、
            // instr との差がそのまま「暇にしていた時間」になる
            self.dbg.executed += 1;
            if self.dbg.trace_cap > 0 {
                let mut bytes = [0u8; 15];
                for (i, b) in bytes.iter_mut().enumerate() {
                    *b = self.read8(lin.wrapping_add(i as u32));
                }
                if self.dbg.trace.len() == self.dbg.trace_cap {
                    self.dbg.trace.pop_front();
                }
                let instr = self.dbg.instr;
                self.dbg.trace.push_back(debug::Step {
                    instr,
                    cs,
                    ip,
                    bytes,
                });
            }
        }

        // 3. BIOS HLE の入口に居るなら、バイト列を実行せずホスト関数で肩代わりする。
        //    OSがIVTを書き換えていればここには来ない。
        //    生ROM実行 (boot_rom) では 0xF000 が実コードなので素通しする
        if self.bios_hle && self.cpu.sregs[cpu::CS] == BIOS_SEG {
            let vec = self.cpu.ip as u8;
            // 完了しなかったサービス (キー待ちなど) はIRETせずに戻る。
            // 次のサイクルで同じINTがやり直され、実BIOSが割り込みを待って
            // 回っているのと同じ状態になる
            if self.bios_interrupt(vec) {
                self.return_flags_to_caller();
                cpu::iret(self, false);
            }
            return;
        }

        // 4. 命令を実行する。TFは**実行前**の値を見る。
        //    ハンドラ内でTFが落ちても、この命令のシングルステップは成立させる
        let tf = self.cpu.flag(cpu::TF);
        self.trap_ip = self.cpu.ip; // 未実装トラップの「犯行現場」用
        self.pending_fault.set(None);
        // フォールトに備えてCPU状態を控える。**実機の約束は「フォールトした
        // 命令は何も起きなかったことになる」** — IPだけ巻き戻して汚れた
        // レジスタを残すと、再実行が汚れの上に積む。実際に `add mem,reg` の
        // 読みがデマンドページングに当たり、フォールトの器 0xFFFFFFFF を
        // 足した EDX (-1) のまま再実行して、muslのELF解析が1バイトずれた
        //
        // ただし**要るときだけ控える**。控えの使い道は #PF の巻き戻し
        // (ページング有効時しか起きない) と、ユーザー空間 (CPL=3) の #UD
        // 巻き戻しの2つ。どちらも起き得ない「ページングOFFかつリング0」—
        // bzImage の解凍ステブや16bit機の全域 — では、毎命令352バイトの
        // 複写が純粋な無駄になる。PGは命令の途中で変わらない (mov cr0 は
        // その後にメモリを触らない) ので、命令の頭の判定で足りる
        // 控えは**実行する側**が入れる (guard_save)。フォールバック経路は毎回、
        // キャッシュ済みuopは「メモリに触るものだけ」— レジスタ間演算やjccは
        // #PFが起き得ないので、352バイトの複写ごと省ける
        self.fault_save_kind = FaultSaveKind::None;
        // デバッガが見ているときは従来経路 (before_exec/トレースの意味を守る)。
        // 普段はデコード済み命令キャッシュ経由 — 対象外は中で従来経路に落ちる
        if self.dbg.on {
            self.guard_save();
            cpu::step(self);
        } else {
            cpu::dcache::step_cached(self, chain_extra);
        }

        // 命令中にページフォールトが起きていたら、**CPUを命令前の姿に戻して**
        // #PF を配送する。ハンドラがページを直して iret すれば、同じ命令が
        // 白紙からやり直される (メモリ側は: フォールトした書き込み自体は
        // 捨ててあり、それ以外の同一命令内の書き込みは再実行が上書きする)
        // ユーザー空間 (CPL=3) の未実装命令は、機械を止めずに #UD として
        // OSへ裁かせる — 実CPUの挙動そのもので、カーネルはそのプロセスだけ
        // SIGILL で殺して先へ進む (Alpineのnlplug-findfsがSSEを使い、
        // マシンごと止まってシェルに届かなかった)。
        // 何が来たかは ud_user に控える — 開発の観測は失わない
        if let Some(t) = &self.trap {
            if self.cpu.cpl() == 3 && self.pending_fault.get().is_none() {
                self.ud_user.insert(t.reason.clone());
                self.trap = None;
                // CPL=3 で実行した命令なら、頭の判定で必ず控えている
                assert!(self.guard_restore(), "CPL=3 の命令に控えが無い");
                cpu::interrupt(self, 6);
                return;
            }
        }

        // セグメント検査 (limit/書込可否) の違反はページングより先に裁く —
        // 実CPUでもセグメンテーションが番地を作ってからページングが訳す順。
        // 毒番地アクセスが偽の #PF/トラップを起こしていても、本当の事件は
        // こちらなので捨てる
        if let Some(vec) = self.pending_seg_fault.take() {
            self.pending_fault.set(None);
            self.trap = None;
            assert!(self.guard_restore(), "セグメント例外なのに控えが無い");
            cpu::seg_fault_err(self, vec, self.cpu.ip, 0);
            return;
        }

        if let Some(f) = self.pending_fault.take() {
            // **フォールトしたページの写しを捨てる。**
            //
            // Linuxは「PTEはもう直したのに#PFが来た」(spurious fault) を想定して
            // いて、ハンドラは何もせず iret し、再実行が通ることを期待する。
            // 古い写しが残っていると同じ#PFが延々と繰り返され、起動が進まなく
            // なる (実際にここで止まった)。実機も #PF のページは無効化する
            self.tlb_flush_page(f.la);
            // フェッチがフォールトすると 0xFF (未マップの器) を命令として
            // デコードし、偽の「未実装」トラップが立つことがある。
            // 本当の事件は #PF の方 — トラップは取り消して配送する
            self.trap = None;
            // #PF はページング有効時にしか起きず、そのときは必ず控えている
            assert!(
                self.guard_restore(),
                "#PF なのに控えが無い (ページングOFFで#PF?)"
            );
            self.cpu.cr2 = f.la;
            let err = (f.present as u32)
                | ((f.write as u32) << 1)
                | (((self.cpu.cpl() == 3) as u32) << 2);
            cpu::page_fault(self, err);
            return;
        }

        // 5. トラップフラグが立っていたら、命令が終わってから INT 1。
        //    「実行してから止まる」ので、デバッガは1命令ずつ進められる
        if tf && !self.halted {
            cpu::interrupt(self, 1);
        }
    }

    /// HLTするか命令数上限まで実行。
    /// 割り込みが保留されていればHLTでも止まらない (目を覚ますため)
    ///
    /// 上限も戻り値も**仮想時間 (TSCの進み)** で数える。忙しいときは
    /// 1命令=1なので従来と同じだが、アイドルの早送りが飛ばした分も含む。
    /// ここを呼んだ回数 (実仕事) で数えると、アイドル中は1回でPIT1周期ぶん
    /// 時間が飛ぶので、同じ予算でゲストの時計が何百倍も速く回ってしまう —
    /// DOSの時計が暴走し、snakeが目で追えない速さになる。
    /// 「予算=仮想時間」にしておけば、呼ぶ側は実時間に合わせて予算を配る
    /// だけで、忙しくても暇でもゲストの時計は同じ速さで流れる。
    pub fn run(&mut self, max_instructions: u64) -> u64 {
        let start_tsc = self.cpu.tsc;
        loop {
            let elapsed = self.cpu.tsc.wrapping_sub(start_tsc);
            if elapsed >= max_instructions {
                return elapsed;
            }
            // HLT中でも装置は動き続ける。タイマ割り込みで目を覚ますため、
            // 「保留が無ければ終わり」ではなく「タイマも止まっていれば終わり」で判定する
            if self.halted && self.pending_irq.is_none() && !self.devices.pit.counters[0].running {
                break;
            }
            if self.trap.is_some() {
                break; // 未実装にぶつかった。生きたまま止まっている
            }
            // 残り予算を渡す — アイドルの早送りが予算を飛び越えないように。
            // 飛び越えると「頼んだ分だけ進んだ」という呼ぶ側の勘定が壊れ、
            // 暇な機械の時計だけが速く回る (ELKS tetris が即死した原因)
            self.step_budgeted(max_instructions - elapsed);
            // デバッガが止めたら抜ける。**見張っていなければ真偽値1つ**なので
            // 計測経路には効かない。
            //
            // ブレークポイントで止まった場合、最後の1回は実行されていない
            // (step が実行前に判定するため)。正確な命令数が要るなら
            // `dbg.instr` を見る — こちらは計測用の概数でよい
            if self.dbg.on && self.dbg.stop.is_some() {
                break;
            }
        }
        self.cpu.tsc.wrapping_sub(start_tsc)
    }

    pub fn console_string(&self) -> String {
        String::from_utf8_lossy(&self.console).into_owned()
    }
}
