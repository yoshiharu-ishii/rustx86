pub mod bios;
pub mod bus;
pub mod bzimage;
pub mod cp437;
pub mod cpu;
pub mod debug;
pub mod dev;
pub mod disk;
pub mod elf;
pub mod snapshot;

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
    /// アイドル (HLT) の早送りが飛ばした仮想命令数の累計。
    ///
    /// **走らせる側が実時間との釣り合いを取るための読み値**で、機械の状態では
    /// ない (スナップショットに入れない)。ランナーはスライスごとにこれを
    /// 読み取って、「飛ばした時間ぶんだけ実時間で待つ」ことでゲストの時計を
    /// 実時間に繋ぎ止める。忙しい実行は自由に速く、暇は実時間どおりに流れる
    pub idle_skipped: u64,
    /// TLB — 線形→物理の変換の写し。**ページングの最大のボトルネックを消す。**
    ///
    /// ページング有効時、変換1回は2段の表 (PDE→PTE) を読む = 物理メモリ2回。
    /// これを毎バイトやると、4バイト読むのに変換4回×表2回=8回の余計な読み。
    /// 実CPUと同じく、一度歩いた結果を控えて次から表を引かない。
    /// 決定的なので写しても結果は同じ — 無効化は mov cr3 / invlpg / cr0 で行う。
    /// `Cell` なのは読み経路 (&self) からも埋めるため
    tlb: Vec<std::cell::Cell<TlbEntry>>,
}

/// TLBの1エントリ。present な変換だけを載せる (不在フォールトは載せない)。
/// 権限 (書ける/ユーザーで触れる) はここに持ち、CPLとWPは引くたびに新しく見る
#[derive(Clone, Copy)]
struct TlbEntry {
    /// 仮想ページ番号 (la >> 12)。`INVALID` は空きスロット
    tag: u32,
    /// 物理ページの4K境界の先頭
    base: u32,
    /// このページは書けるか (PDEとPTEのR/Wが両方立っている)
    writable: bool,
    /// ユーザー (リング3) が触れるか (PDEとPTEのU/Sが両方立っている)
    user_ok: bool,
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
            idle_skipped: 0,
            tlb: (0..TLB_SLOTS)
                .map(|_| {
                    std::cell::Cell::new(TlbEntry {
                        tag: TLB_INVALID,
                        base: 0,
                        writable: false,
                        user_ok: false,
                    })
                })
                .collect(),
        }
    }

    /// TLBを全部空にする。mov cr3 (アドレス空間の切り替え) や CR0 の変更、
    /// スナップショット復元の後に呼ぶ。**表を書き換えたのに写しが古いと
    /// 幽霊のページが見え続ける**ので、切り替えの合図で必ず捨てる
    pub fn tlb_flush(&self) {
        for slot in &self.tlb {
            let mut e = slot.get();
            e.tag = TLB_INVALID;
            slot.set(e);
        }
    }

    /// TLBの1ページだけ無効化する (INVLPG)。ページテーブルの1エントリを
    /// 書き換えたカーネルは、この命令でそのページの写しだけを捨てる
    pub fn tlb_flush_page(&self, la: u32) {
        let slot = ((la >> 12) as usize) & (TLB_SLOTS - 1);
        let mut e = self.tlb[slot].get();
        e.tag = TLB_INVALID;
        self.tlb[slot].set(e);
    }

    /// RAMのバイト数 (= 実際の確保量)
    pub fn ram_bytes(&self) -> usize {
        self.mem.len()
    }

    /// 物理アドレスへ書く (変換しない)。テストや装置初期化用
    pub fn write_phys8(&mut self, pa: u32, val: u8) {
        if let Some(b) = self.mem.get_mut(pa as usize) {
            *b = val;
        }
        // 超えたら捨てる (未マップへの書き込みは実機でも消える)
    }

    pub fn write_phys32(&mut self, pa: u32, val: u32) {
        for (i, b) in val.to_le_bytes().iter().enumerate() {
            self.write_phys8(pa.wrapping_add(i as u32), *b);
        }
    }

    /// ブートセクタ (512バイト) を0x7C00に配置し、CS:IP=0000:7C00から実行開始
    pub fn load_boot_sector(&mut self, sector: &[u8]) -> Result<(), String> {
        if sector.len() != 512 {
            return Err(format!(
                "boot sector must be 512 bytes, got {}",
                sector.len()
            ));
        }
        if sector[510] != 0x55 || sector[511] != 0xAA {
            return Err("missing boot signature 0x55AA".into());
        }
        self.power_on_self_test();
        self.mem[0x7C00..0x7E00].copy_from_slice(sector);
        self.cpu.set_cs_ip(0x0000, 0x7C00);
        self.cpu.regs[cpu::DX] = 0x0080; // DL = ブートドライブ番号
        Ok(())
    }

    /// ハードウェア割り込みベクタを直接立てる (PICを介さない経路。テスト用)
    pub fn raise_irq(&mut self, vector: u8) {
        self.pending_irq = Some(vector);
    }

    /// 装置を進め、挙手があればPICへ渡す。
    ///
    /// **一周はこうなっている**:
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
    fn idle_fast_forward(&mut self) {
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
        let ticks = clocks.div_ceil(PIT_CLOCKS_PER_TICK).max(1);
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

    /// ディスクイメージを入れ、その先頭セクタからブートする
    pub fn boot_from_disk(&mut self, image: Vec<u8>) -> Result<(), String> {
        let d = Disk::from_image(image)?;
        let boot = d.read_sector(0).ok_or("ブートセクタが読めない")?.to_vec();
        self.disk = Some(d);
        self.power_on_self_test();
        self.mem[0x7C00..0x7E00].copy_from_slice(&boot);
        self.cpu.set_cs_ip(0x0000, 0x7C00);
        self.cpu.regs[cpu::DX] = 0x0000; // DL = 0 (フロッピーA)
        Ok(())
    }

    /// bzImage を直接ロードして 32bit カーネルエントリへ飛ぶ (Tier 3b)。
    ///
    /// ブートローダ (GRUB) がやることを肩代わりする「32bit ブートプロトコル」:
    ///   1. カーネル本体を物理 1MB へ置く
    ///   2. zero page (boot_params) を組んで、cmdline と e820 を入れる
    ///   3. **フラットな32bit protected mode・paging off** の状態を作る
    ///   4. `%esi` = zero page の物理番地、`code32_start` へジャンプ
    ///
    /// GDTを組んで far jump…という手順は踏まず、**隠しレジスタに直接
    /// フラットセグメント (base=0, limit=4GB, 32bit) を書く**。実機の
    /// ブートローダが GDT を経て到達する状態を、こちらは結果だけ作れる。
    ///
    /// カーネルは早々にこの状態を捨てて自前のGDT/ページテーブルを作るので、
    /// ここで渡すのは「最初の一歩を踏み出せる姿勢」だけでよい
    pub fn boot_bzimage(&mut self, image: &[u8], cmdline: &str) -> Result<(), String> {
        self.boot_bzimage_with_initrd(image, cmdline, None)
    }

    /// Linux を起動する (bzImage / vmlinux の自動判別)。
    ///
    /// 先頭が ELF なら vmlinux 直接ロード (解凍ステブ無し = 起動が半分)、
    /// そうでなければ bzImage。呼ぶ側はファイルの中身を気にしなくてよい
    pub fn boot_linux_with_initrd(
        &mut self,
        image: &[u8],
        cmdline: &str,
        initrd: Option<&[u8]>,
    ) -> Result<(), String> {
        if elf::is_elf(image) {
            self.boot_vmlinux_with_initrd(image, cmdline, initrd)
        } else {
            self.boot_bzimage_with_initrd(image, cmdline, initrd)
        }
    }

    /// 非圧縮の vmlinux (ELF32) を直接ロードして起動する。
    ///
    /// bzImage の自己解凍ステブは**起動全体の55% (540M命令) を無言で食う**。
    /// 展開済みのカーネルをこちらで物理メモリに置けば、その区間は丸ごと消える。
    /// Firecracker が bzImage ではなく vmlinux を要求するのと同じ判断。
    /// vmlinux は tools/extract-vmlinux.sh で bzImage から取り出せる
    pub fn boot_vmlinux_with_initrd(
        &mut self,
        image: &[u8],
        cmdline: &str,
        initrd: Option<&[u8]>,
    ) -> Result<(), String> {
        let v = elf::parse_vmlinux(image)?;
        self.power_on_self_test();

        // セグメントを物理メモリへ。解凍ステブがやっていた仕事の代行:
        // ファイルの中身を写し、memsz までの残り (BSS) をゼロで埋める
        for s in &v.segments {
            let end = s.paddr as usize + s.memsz;
            if end > self.mem.len() {
                return Err(format!(
                    "vmlinux のセグメント (物理 0x{:08x}..0x{end:08x}) がRAM ({}MB) に収まらない",
                    s.paddr,
                    self.mem.len() >> 20
                ));
            }
            let dst = s.paddr as usize;
            self.mem[dst..dst + s.filesz].copy_from_slice(&image[s.offset..s.offset + s.filesz]);
            self.mem[dst + s.filesz..end].fill(0);
        }

        // zero page に写すセットアップヘッダが vmlinux には無いので合成する。
        // カーネルが読み返して意味を持つ欄だけ: マジック・版・LOADED_HIGH。
        // (type_of_loader / ramdisk / cmdline は build_zero_page 自身が書く)
        let mut hdr_src = vec![0u8; 0x268];
        hdr_src[0x202..0x206].copy_from_slice(b"HdrS");
        hdr_src[0x206..0x208].copy_from_slice(&0x020Cu16.to_le_bytes());
        hdr_src[0x211] = 0x01; // LOADED_HIGH
        hdr_src[0x214..0x218].copy_from_slice(&v.entry.to_le_bytes());

        self.finish_linux_boot(&hdr_src, cmdline, initrd, v.entry)
    }

    /// initrd (initramfs) 付きの bzImage 起動。
    /// initrd は**RAMの高い方**に置く — カーネル本体 (1MB〜) と展開作業域から
    /// 遠ざけるのが慣習で、実ブートローダも同じことをする
    pub fn boot_bzimage_with_initrd(
        &mut self,
        image: &[u8],
        cmdline: &str,
        initrd: Option<&[u8]>,
    ) -> Result<(), String> {
        let hdr = bzimage::SetupHeader::parse(image)?;
        if !hdr.loaded_high() {
            return Err("LOADED_HIGH でない (bzImage ではなく zImage?)".into());
        }

        self.power_on_self_test();

        // カーネル本体を物理 1MB へ。bzImage の kernel_offset 以降が本体
        let kbody = &image[hdr.kernel_offset().min(image.len())..];
        const KERNEL_BASE: u32 = 0x0010_0000;
        for (i, b) in kbody.iter().enumerate() {
            self.write_phys8(KERNEL_BASE + i as u32, *b);
        }

        self.finish_linux_boot(image, cmdline, initrd, hdr.code32_start)
    }

    /// Linux 起動の共通の尾部 — カーネル本体を置いた後の仕事。
    /// cmdline / initrd / zero page を配り、フラット32bitの姿勢を作って
    /// `entry` へ飛ぶ。bzImage と vmlinux の両経路がここへ合流する。
    /// `hdr_src` は zero page に写すセットアップヘッダの持ち主
    /// (bzImage ならファイル先頭、vmlinux なら合成したもの)
    fn finish_linux_boot(
        &mut self,
        hdr_src: &[u8],
        cmdline: &str,
        initrd: Option<&[u8]>,
        entry: u32,
    ) -> Result<(), String> {
        use cpu::{CS, DS, ES, FS, GS, SS};

        // cmdline を低位に置く (慣習の 0x2_0000)
        const CMDLINE_ADDR: u32 = 0x0002_0000;
        for (i, b) in cmdline.bytes().enumerate() {
            self.write_phys8(CMDLINE_ADDR + i as u32, b);
        }
        self.write_phys8(CMDLINE_ADDR + cmdline.len() as u32, 0);

        // initrd をRAM上端寄り (1MBの余白を残してページ整列) に置く
        let initrd_loc = match initrd {
            Some(data) => {
                let size = data.len() as u32;
                let top = self.mem.len() as u32;
                if size + 0x0100_0000 > top {
                    return Err(format!(
                        "initrd ({size} バイト) がRAM ({top} バイト) に収まらない"
                    ));
                }
                let addr = (top - size - 0x0010_0000) & !0xFFF;
                for (i, b) in data.iter().enumerate() {
                    self.write_phys8(addr + i as u32, *b);
                }
                Some((addr, size))
            }
            None => None,
        };

        // zero page を組んで低位に置く (慣習の 0x1_0000)
        const ZERO_PAGE_ADDR: u32 = 0x0001_0000;
        let zp = bzimage::build_zero_page(hdr_src, self.mem.len() as u64, CMDLINE_ADDR, initrd_loc);
        for (i, b) in zp.iter().enumerate() {
            self.write_phys8(ZERO_PAGE_ADDR + i as u32, *b);
        }

        // --- 実機のブートローダが作る GDT を、物理メモリに組む ---
        //
        // 隠しレジスタに直接書くショートカットは、**カーネルがセグメントを
        // 再ロードするまでしか保たない**。カーネルは起動直後に mov ds,ax 等で
        // セグメントを触り、そのとき GDTR の指す表を読み直す。表が無いと
        // ゴミを記述子として読んで base が壊れ、墜落する (実際に踏んだ)。
        //
        // Linux boot protocol の要求どおり、flat な GDT を用意する:
        //   index 2 (selector 0x10) = flat 32bit code
        //   index 3 (selector 0x18) = flat 32bit data
        const GDT_ADDR: u32 = 0x0000_0800;
        // 8バイトの記述子。base=0, limit=0xFFFFF(4Kページ単位で4GB), access, flags
        let desc = |access: u8| -> [u8; 8] { [0xFF, 0xFF, 0, 0, 0, access, 0xCF, 0] };
        let mut gdt = [0u8; 32]; // 4エントリ
        gdt[16..24].copy_from_slice(&desc(0x9A)); // 0x10: code (P,DPL0,code,readable)
        gdt[24..32].copy_from_slice(&desc(0x92)); // 0x18: data (P,DPL0,data,writable)
        for (i, b) in gdt.iter().enumerate() {
            self.write_phys8(GDT_ADDR + i as u32, *b);
        }
        self.cpu.gdtr_base = GDT_ADDR;
        self.cpu.gdtr_limit = 31;

        // PE を立ててから、GDT経由でセグメントをロードする。
        // load_seg が GDT から隠しレジスタへ写すので、以後カーネルが
        // 同じセレクタを mov し直しても同じ記述子が読める
        self.cpu.cr0 |= 1; // PE (PG は立てない)
        cpu::load_seg_pub(self, CS, 0x10);
        for s in [DS, ES, FS, GS, SS] {
            cpu::load_seg_pub(self, s, 0x18);
        }

        // 規約: %esi = zero page、エントリへ
        self.cpu.regs[cpu::SI] = ZERO_PAGE_ADDR;
        self.cpu.set_ip(entry);
        self.cpu.set_flag(cpu::IF, false); // カーネルが自分でSTIするまで割り込み禁止
        Ok(())
    }

    /// 線形アドレスから読む。**ページングが有効ならここで物理へ変換する**。
    /// CPUが触るのはこちら (呼び出し側は線形アドレスを渡す)
    /// 未実装にぶつかった。**その命令を実行する前の CS:IP** で止める。
    /// 呼び出し側は直後に return して、命令を実行しないこと
    pub(crate) fn trap(&mut self, reason: String) {
        self.trap = Some(Trap {
            reason,
            cs: self.cpu.sregs[cpu::CS],
            ip: self.trap_ip,
        });
    }

    pub fn read8(&self, addr: u32) -> u8 {
        match self.translate_for(addr, false) {
            Ok(pa) => self.read_phys8(pa),
            Err(f) => {
                self.note_fault(f);
                0xFF // フォールトした読みの器。命令の終わりに#PFで巻き戻す
            }
        }
    }

    /// 物理アドレスから読む (変換しない)。ページテーブルの歩きと、
    /// 物理番地で語る装置・テストが使う
    pub fn read_phys8(&self, pa: u32) -> u8 {
        // RAMを超えた番地は未マップ。実機のバスと同じく 0xFF を返す (折り返さない)。
        // リアルモードのアドレスは cpu::lin が 1MB に丸めてから来るので、
        // 16bit機 (1MB) でここが 0xFF を返すことはない
        *self.mem.get(pa as usize).unwrap_or(&0xFF)
    }

    pub fn read_phys32(&self, pa: u32) -> u32 {
        // RAMに収まるなら4バイトを一気に読む (ページウォークの熱い経路)
        let a = pa as usize;
        if a + 4 <= self.mem.len() {
            u32::from_le_bytes([
                self.mem[a],
                self.mem[a + 1],
                self.mem[a + 2],
                self.mem[a + 3],
            ])
        } else {
            u32::from_le_bytes([
                self.read_phys8(pa),
                self.read_phys8(pa.wrapping_add(1)),
                self.read_phys8(pa.wrapping_add(2)),
                self.read_phys8(pa.wrapping_add(3)),
            ])
        }
    }

    /// 線形アドレスを物理アドレスへ。
    ///
    /// **ここがページングの正体**である。CR0.PGが立っていなければ線形=物理。
    /// 立っていれば、上位20bitで2段の表を引く:
    ///   線形 [31:22]=ディレクトリ番号 [21:12]=テーブル番号 [11:0]=ページ内オフセット
    ///
    /// TLB (変換の写し) はまだ持たない。決定的なので**毎回歩いても結果は同じ**で、
    /// 速度が問題になるまで足さない (「測ってから足す」— docs/ci.md と同じ流儀)。
    ///
    /// こちらは**寛容な版** (デバッガ・ツール用)。未マップは RAM 外の番地を
    /// 返し、読めば 0xFF になる。CPUの実行経路は [`translate_for`] を使い、
    /// 失敗を #PF として配送する
    pub fn translate(&self, la: u32) -> u32 {
        self.translate_for(la, false).unwrap_or(0xFFFF_FFFF)
    }

    /// CPUのアクセス経路の変換。**ページ保護もここで裁く**:
    ///   - present が無ければ不在フォールト
    ///   - 書き込みで R/W=0 のページは、CR0.WP (リング0でも守る) か
    ///     リング3なら保護フォールト。カーネルはこの挙動を起動時に試験し、
    ///     #PFが来ないと「壊れたWP」として起動を拒否する (実際に拒否された)
    pub fn translate_for(&self, la: u32, write: bool) -> Result<u32, PageFault> {
        if self.cpu.cr0 & 0x8000_0000 == 0 {
            return Ok(la); // PG off: 線形がそのまま物理
        }
        // --- TLBを引く。当たれば表を歩かない ---
        let vpn = la >> 12;
        let slot = (vpn as usize) & (TLB_SLOTS - 1);
        let e = self.tlb[slot].get();
        let (base, writable, user_ok) = if e.tag == vpn {
            (e.base, e.writable, e.user_ok)
        } else {
            // ミス: 表を歩いて present なら控える。**権限ビットも一緒に控える**が、
            // 「今この瞬間に許されるか」の判定 (CPL/WP) は下で新しく見る
            let (base, writable, user_ok) = self.walk_page(la)?;
            self.tlb[slot].set(TlbEntry {
                tag: vpn,
                base,
                writable,
                user_ok,
            });
            (base, writable, user_ok)
        };
        // --- 権限チェック。CPLとWPは引くたびに新しく (sys_accessも) ---
        let user = self.cpu.cpl() == 3 && !self.sys_access.get();
        let wp = self.cpu.cr0 & 0x0001_0000 != 0;
        if write && !writable && (user || wp) {
            return Err(PageFault {
                la,
                write,
                present: true,
            });
        }
        if user && !user_ok {
            return Err(PageFault {
                la,
                write,
                present: true,
            });
        }
        Ok(base | (la & 0xFFF))
    }

    /// 2段の表を歩いて、ページの物理先頭と権限ビットを返す (TLBミス時のみ)。
    /// 返すのは (4K境界の物理先頭, 書けるか, ユーザーで触れるか)。
    /// **不在は Err(present:false)** — これは TLB に載せない (次回また歩く)
    fn walk_page(&self, la: u32) -> Result<(u32, bool, bool), PageFault> {
        let notp = || PageFault {
            la,
            write: false,
            present: false,
        };
        let dir = (la >> 22) & 0x3FF;
        let pde = self.read_phys32((self.cpu.cr3 & !0xFFF) + dir * 4);
        if pde & 1 == 0 {
            return Err(notp());
        }
        if pde & 0x80 != 0 {
            // 4MBページ (PSE): テーブルを引かず、ディレクトリで直に物理が決まる。
            // TLBは4K単位なので、この4Kぶんの物理先頭を作る
            let base = (pde & 0xFFC0_0000) | (la & 0x003F_F000);
            return Ok((base, pde & 2 != 0, pde & 4 != 0));
        }
        let tbl = (la >> 12) & 0x3FF;
        let pte = self.read_phys32((pde & !0xFFF) + tbl * 4);
        if pte & 1 == 0 {
            return Err(notp());
        }
        // R/W・U/S は2段の**厳しい方**が効く (両方立って初めて許す)
        let writable = pde & 2 != 0 && pte & 2 != 0;
        let user_ok = pde & 4 != 0 && pte & 4 != 0;
        Ok((pte & !0xFFF, writable, user_ok))
    }

    /// 変換失敗を記録する (最初の1件だけ)。命令の終わりで #PF になる
    fn note_fault(&self, f: PageFault) {
        if self.pending_fault.get().is_none() {
            self.pending_fault.set(Some(f));
        }
    }

    /// REPの一括処理用: 線形アドレス `la` から、**同じページ内で連続して
    /// 触れる物理範囲**を返す。`write` は書き込みか。
    /// 返り値は (物理先頭, そのページで残るバイト数)。フォールトなら None
    /// (呼び出し側が note_fault 済みのつもりで巻き戻す)。
    /// RAMを超える範囲は None (遅い道に落とす)
    pub(crate) fn phys_span(&self, la: u32, write: bool) -> Option<(usize, usize)> {
        let pa = match self.translate_for(la, write) {
            Ok(pa) => pa,
            Err(f) => {
                self.note_fault(f);
                return None;
            }
        };
        let page_remain = 0x1000 - (la & 0xFFF) as usize;
        let a = pa as usize;
        if a + page_remain > self.mem.len() {
            return None;
        }
        Some((a, page_remain))
    }

    /// 生のメモリスライスへの参照 (REP一括処理の宛先)。
    /// VRAMやデバッガの都合は呼び出し側が事前に外す
    pub(crate) fn mem_slice_mut(&mut self) -> &mut [u8] {
        &mut self.mem
    }

    /// メモリ書き込み。
    ///
    /// テキストVRAMは**メモリ空間に居座る装置**なので、素通しで `mem` に書く。
    /// 実機でもビデオカードのRAMがCPUのアドレス空間に窓として現れているだけで、
    /// 書き込み経路に特別な変換は無い。ここで足しているのは描画側への合図だけ。
    ///
    /// 読み出し ([`read8`](Self::read8)) には一切分岐を入れていない。
    /// メモリアクセスは最も回数の多い経路なので、**書き込み側だけで済む
    /// 仕掛けなら書き込み側に寄せる**。
    pub fn write8(&mut self, addr: u32, val: u8) {
        // 線形→物理。以後の VRAM 判定もデバッガも**物理番地**で語る
        // (VRAMは物理アドレス空間の窓なので、そこに写像された線形から書いても
        //  正しく dirty が立つ)
        let a = match self.translate_for(addr, true) {
            Ok(pa) => pa,
            Err(f) => {
                self.note_fault(f);
                return; // フォールトした書き込みは実行しない (再実行で改めて書く)
            }
        } as usize;
        if a >= self.mem.len() {
            return; // RAMを超えた書き込みは捨てる
        }
        // デバッガを切っていれば真偽値1つで抜ける。**最も回数の多い経路**なので
        // 見張る番地の集合を引く前に元締めで落とす
        if self.dbg.on && self.dbg.mem_write.contains(&(a as u32)) {
            self.dbg.stop = Some(debug::Stop::WriteMem {
                addr: a as u32,
                old: self.mem[a],
                new: val,
                at: self.dbg.at,
            });
        }
        self.mem[a] = val;
        if (bus::VRAM_TEXT_BASE as usize..=bus::VRAM_TEXT_END as usize).contains(&a) {
            self.vram_dirty = true;
        }
    }

    pub fn read16(&self, addr: u32) -> u16 {
        // ページ内に収まるなら**1回の変換で2バイト**読む。
        // ページ跨ぎ (稀) のときだけバイトごとに落とす
        if addr & 0xFFF <= 0xFFE {
            match self.translate_for(addr, false) {
                Ok(pa) => {
                    let a = pa as usize;
                    if a + 2 <= self.mem.len() {
                        return self.mem[a] as u16 | (self.mem[a + 1] as u16) << 8;
                    }
                    0xFFFF
                }
                Err(f) => {
                    self.note_fault(f);
                    0xFFFF
                }
            }
        } else {
            self.read8(addr) as u16 | (self.read8(addr.wrapping_add(1)) as u16) << 8
        }
    }

    pub fn read32(&self, addr: u32) -> u32 {
        // ページ内に収まるなら**1回の変換で4バイト**読む
        if addr & 0xFFF <= 0xFFC {
            match self.translate_for(addr, false) {
                Ok(pa) => self.read_phys32(pa),
                Err(f) => {
                    self.note_fault(f);
                    0xFFFF_FFFF
                }
            }
        } else {
            self.read16(addr) as u32 | (self.read16(addr.wrapping_add(2)) as u32) << 16
        }
    }

    pub fn write32(&mut self, addr: u32, val: u32) {
        if addr & 0xFFF <= 0xFFC && self.write_wide(addr, val, 4) {
            return;
        }
        self.write16(addr, val as u16);
        self.write16(addr.wrapping_add(2), (val >> 16) as u16);
    }

    pub fn write16(&mut self, addr: u32, val: u16) {
        if addr & 0xFFF <= 0xFFE && self.write_wide(addr, val as u32, 2) {
            return;
        }
        self.write8(addr, val as u8);
        self.write8(addr.wrapping_add(1), (val >> 8) as u8);
    }

    /// ページ内に収まる2/4バイト書き込みを**1回の変換**で行う。
    /// 成功したら true。フォールト・跨ぎ・見張り対象などで速い道を使えないときは
    /// false を返し、呼び出し側がバイトごとの道へ落とす
    fn write_wide(&mut self, addr: u32, val: u32, width: u32) -> bool {
        let pa = match self.translate_for(addr, true) {
            Ok(pa) => pa,
            Err(f) => {
                self.note_fault(f);
                return true; // フォールトは「書かない」で完了 (再実行が改めて書く)
            }
        };
        let a = pa as usize;
        if a + width as usize > self.mem.len() {
            return true; // RAM超えは捨てる (完了扱い)
        }
        // デバッガが見張っている、または VRAM に落ちるなら、遅い道で
        // バイトごとの合図を出す (ここは熱くない)
        if self.dbg.on || (bus::VRAM_TEXT_BASE..=bus::VRAM_TEXT_END).contains(&(a as u32)) {
            return false;
        }
        for i in 0..width as usize {
            self.mem[a + i] = (val >> (i * 8)) as u8;
        }
        true
    }

    // --- I/Oポート空間の振り分け ---

    /// ポートから読む。
    ///
    /// 未接続のポートは **0xFF** を返す。実機のISAバスは誰もドライブしないと
    /// プルアップで全ビットが立つためで、OSはこの値を見て「装置が居ない」と
    /// 判断する。ここで panic すると装置探索の段階で止まってしまう
    pub fn io_read8(&mut self, port: u16) -> u8 {
        let val = self.io_read8_inner(port);
        // **読んだ値まで残す。** 「装置が何を答えたか」が分からないと、
        // OSがなぜその判断をしたのかを追えない
        if self.dbg.on && self.dbg.io_read.contains(&port) {
            self.dbg.stop = Some(debug::Stop::ReadIo {
                port,
                val,
                at: self.dbg.at,
            });
        }
        val
    }

    fn io_read8_inner(&mut self, port: u16) -> u8 {
        match bus::decode_io(port) {
            IoTarget::Pic { slave } => {
                let p = &self.devices.pic[slave as usize];
                if port & 1 == 0 {
                    p.read_command()
                } else {
                    p.read_data()
                }
            }
            IoTarget::Pit => {
                let idx = (port & 3) as usize;
                if idx == 3 {
                    0xFF
                } else {
                    self.devices.pit.read_counter(idx)
                }
            }
            IoTarget::Keyboard => {
                if port == 0x64 {
                    self.devices.keyboard.read_status()
                } else {
                    self.devices.keyboard.read_data()
                }
            }
            IoTarget::Uart => self.devices.uart.read(port & 7),
            IoTarget::Cmos => {
                if port == 0x71 {
                    self.devices.cmos.read_data()
                } else {
                    0xFF
                }
            }
            IoTarget::Crtc => {
                if port == 0x3D5 {
                    self.devices.crtc.read_data()
                } else {
                    0xFF
                }
            }
            IoTarget::SystemControl => {
                // bit4 をトグルし続ける。OSがリフレッシュ矩形波を数えて
                // 時間を測る古い手口に付き合うため
                self.devices.sysctl ^= 0x10;
                self.devices.sysctl
            }
            IoTarget::Unmapped => {
                self.unhandled_io.insert(port);
                0xFF
            }
        }
    }

    pub fn io_write8(&mut self, port: u16, val: u8) {
        if self.dbg.on && self.dbg.io_write.contains(&port) {
            self.dbg.stop = Some(debug::Stop::WriteIo {
                port,
                val,
                at: self.dbg.at,
            });
        }
        match bus::decode_io(port) {
            IoTarget::Pic { slave } => {
                let p = &mut self.devices.pic[slave as usize];
                if port & 1 == 0 {
                    p.write_command(val)
                } else {
                    p.write_data(val)
                }
            }
            IoTarget::Pit => {
                let idx = (port & 3) as usize;
                if idx == 3 {
                    self.devices.pit.write_control(val)
                } else {
                    self.devices.pit.write_counter(idx, val)
                }
            }
            IoTarget::Keyboard => {
                if port == 0x64 {
                    self.devices.keyboard.write_command(val)
                } else {
                    self.devices.keyboard.write_data(val)
                }
            }
            IoTarget::Uart => self.devices.uart.write(port & 7, val),
            IoTarget::Cmos => {
                if port == 0x70 {
                    self.devices.cmos.write_index(val)
                } else {
                    self.devices.cmos.write_data(val)
                }
            }
            IoTarget::Crtc => {
                if port == 0x3D4 {
                    self.devices.crtc.write_index(val)
                } else {
                    // 表示開始位置が動いたら、メモリは変わらなくても**画面は変わる**
                    if matches!(self.devices.crtc.index(), 0x0C | 0x0D) {
                        self.vram_dirty = true;
                    }
                    self.devices.crtc.write_data(val)
                }
            }
            IoTarget::SystemControl => self.devices.sysctl = val,
            IoTarget::Unmapped => {
                self.unhandled_io.insert(port);
            }
        }
    }

    // --- テキストVRAM ---

    /// テキスト画面の生バイト列 (80×25、文字と属性が交互)。
    ///
    /// **先頭から4000バイトではなく、CRTCが指す位置から4000バイトを返す。**
    ///
    /// テキストVRAMの窓は32KBあり、80x25の1画面はそのうち4000バイトでしかない。
    /// どこから表示するかを決めるのはCRTCのレジスタ 0x0C/0x0D で、ここを動かすと
    /// **メモリを1バイトも書き換えずに画面をスクロールできる** (ハードウェアスクロール)。
    /// 80年代の機械が遅いCPUで滑らかにスクロールできたのはこの仕組みによる。
    ///
    /// これを見ずに常に先頭を返していたため、CGA向けにハードウェアスクロールで
    /// 描くソフト (zmiy など) は**画面の下が永久に出てこなかった**。
    /// CRTCは実装してあり、説明にも「ここを動かすとスクロールできる」と
    /// 書いてあったのに、**描く側が見ていなかった**。
    pub fn text_vram(&self) -> &[u8] {
        let win = (bus::VRAM_TEXT_END - bus::VRAM_TEXT_BASE + 1) as usize;
        // 開始位置は文字単位。1文字2バイトなので倍にする
        let start = (self.devices.crtc.start_offset() as usize * bus::TEXT_CELL) % win;
        let b = bus::VRAM_TEXT_BASE as usize + start;
        // 窓の端をまたぐ場合は、素直に先頭を返す (実機は巻き戻るが、
        // そこまで使うソフトは見ていない。使うものが出てきたら組み立てる)
        if start + bus::TEXT_LEN <= win {
            &self.mem[b..b + bus::TEXT_LEN]
        } else {
            let base = bus::VRAM_TEXT_BASE as usize;
            &self.mem[base..base + bus::TEXT_LEN]
        }
    }

    /// 機械の状態をまるごと書き出す。
    ///
    /// **CPUだけでは足りない。** PICのマスクが失われれば以後の割り込みが
    /// 来なくなり、PITのカウンタが戻れば時計が飛ぶ。装置もメモリも
    /// ディスクも含めて初めて「あの瞬間から再開」ができる。
    pub fn save_state(&self) -> Vec<u8> {
        let mut w = snapshot::Writer::new();
        snapshot::write_header(&mut w);

        // CPU
        for r in self.cpu.regs {
            w.u32(r);
        }
        for s in self.cpu.sregs {
            w.u16(s);
        }
        w.u32(self.cpu.ip);
        w.u32(self.cpu.flags);
        // プロテクトモードの状態 (v2)。隠しレジスタを落とすと、復元した瞬間に
        // 全アドレスが嘘になる — セレクタだけでは base を再構成できない
        w.u32(self.cpu.cr0);
        w.u32(self.cpu.gdtr_base);
        w.u16(self.cpu.gdtr_limit);
        w.u32(self.cpu.idtr_base);
        w.u16(self.cpu.idtr_limit);
        w.u16(self.cpu.tr_sel);
        w.u32(self.cpu.tr_base);
        w.u32(self.cpu.tr_limit);
        w.u32(self.cpu.cr2);
        w.u32(self.cpu.cr3);
        w.u32(self.cpu.cr4); // v7
        w.u16(self.cpu.ldtr_sel); // v7
        for d in self.cpu.dr {
            w.u32(d);
        }
        w.u16(self.cpu.fpu_cw); // v7
        w.u32(self.cpu.mxcsr); // v7
        for x in self.cpu.xmm {
            w.u32(x as u32);
            w.u32((x >> 32) as u32);
            w.u32((x >> 64) as u32);
            w.u32((x >> 96) as u32);
        }
        w.u32(self.cpu.tsc as u32); // v7 (下位のみ。较正はやり直せるので十分)
        w.u32((self.cpu.tsc >> 32) as u32);
        for h in self.cpu.hidden {
            w.u32(h.base);
            w.u32(h.limit);
            w.u8(h.access);
            w.bool(h.big);
        }

        // 機械の進行状態
        w.bool(self.halted);
        w.opt_u8(self.pending_irq);

        // 装置
        for p in &self.devices.pic {
            p.save(&mut w);
        }
        self.devices.pit.save(&mut w);
        self.devices.uart.save(&mut w);
        self.devices.keyboard.save(&mut w);
        self.devices.cmos.save(&mut w);
        self.devices.crtc.save(&mut w);

        // メモリとディスク (ほとんどがゼロなので連長圧縮で潰れる)
        w.rle(&self.mem);
        match &self.disk {
            Some(d) => {
                w.bool(true);
                w.rle(&d.data);
            }
            None => w.bool(false),
        }
        w.buf
    }

    /// 書き出した状態へ戻す。
    ///
    /// 途中で失敗すると**半端に書き換わった機械**が残るので、
    /// まず新しい機械の上に組み立ててから丸ごと差し替える
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        let mut m = Machine::new();
        let mut r = snapshot::Reader::new(data);
        snapshot::read_header(&mut r)?;

        for i in 0..8 {
            m.cpu.regs[i] = r.u32()?;
        }
        for i in 0..6 {
            m.cpu.sregs[i] = r.u16()?;
        }
        m.cpu.ip = r.u32()?;
        m.cpu.flags = r.u32()?;
        m.cpu.cr0 = r.u32()?;
        m.cpu.gdtr_base = r.u32()?;
        m.cpu.gdtr_limit = r.u16()?;
        m.cpu.idtr_base = r.u32()?;
        m.cpu.idtr_limit = r.u16()?;
        m.cpu.tr_sel = r.u16()?;
        m.cpu.tr_base = r.u32()?;
        m.cpu.tr_limit = r.u32()?;
        m.cpu.cr2 = r.u32()?;
        m.cpu.cr3 = r.u32()?;
        m.cpu.cr4 = r.u32()?; // v7
        m.cpu.ldtr_sel = r.u16()?; // v7
        for i in 0..8 {
            m.cpu.dr[i] = r.u32()?;
        }
        m.cpu.fpu_cw = r.u16()?; // v7
        m.cpu.mxcsr = r.u32()?; // v7
        for i in 0..8 {
            let a = r.u32()? as u128;
            let b = r.u32()? as u128;
            let c = r.u32()? as u128;
            let d = r.u32()? as u128;
            m.cpu.xmm[i] = a | b << 32 | c << 64 | d << 96;
        }
        m.cpu.tsc = r.u32()? as u64 | ((r.u32()? as u64) << 32);
        for i in 0..6 {
            m.cpu.hidden[i] = cpu::SegHidden {
                base: r.u32()?,
                limit: r.u32()?,
                access: r.u8()?,
                big: r.bool()?,
            };
        }

        m.halted = r.bool()?;
        m.pending_irq = r.opt_u8()?;
        // pic_service は派生状態なのでPICから作り直す

        for i in 0..2 {
            m.devices.pic[i].load(&mut r)?;
        }
        m.devices.pit.load(&mut r)?;
        m.devices.uart.load(&mut r)?;
        m.devices.keyboard.load(&mut r)?;
        m.devices.cmos.load(&mut r)?;
        m.devices.crtc.load(&mut r)?;

        // メモリのRLEはサイズを暗黙に持つ。復元した長さがそのままRAMサイズ。
        // 物理マスクは mem.len() を見るので、これで大きい機械もそのまま復元される。
        // 別マシンとして復元したことを覗き窓に映すため profile も合わせる
        let mem = r.rle()?;
        if !mem.len().is_power_of_two() {
            return Err(format!("RAMサイズが2の冪でない ({})", mem.len()));
        }
        m.profile = if mem.len() == MEM_SIZE {
            MachineProfile::PC_16BIT
        } else {
            MachineProfile {
                name: "32bit PC",
                ram_bytes: mem.len(),
                has_fpu: true,
                has_cpuid: true,
            }
        };
        m.pic_service = m.devices.pic[0].has_pending();
        m.tlb_flush(); // 復元でメモリもcr3も総入れ替え — 古い写しは無効
        m.mem = mem;
        m.disk = if r.bool()? {
            Some(Disk::from_image(r.rle()?)?)
        } else {
            None
        };

        *self = m;
        Ok(())
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
        self.write16(addr, (stacked & !keep) | (self.cpu.flags as u16 & keep));
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
    pub fn step(&mut self) {
        // 未実装で止まっていたら、以後は何もしない (run系がここで抜ける)
        if self.trap.is_some() {
            return;
        }
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
            self.idle_fast_forward();
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
                let mut bytes = [0u8; 5];
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
        //    OSがIVTを書き換えていればここには来ない
        if self.cpu.sregs[cpu::CS] == BIOS_SEG {
            let vec = self.cpu.ip as u8;
            // 完了しなかったサービス (キー待ちなど) はIRETせずに戻る。
            // 次のサイクルで同じINTがやり直され、実BIOSが割り込みを待って
            // 回っているのと同じ状態になる
            if self.bios_interrupt(vec) {
                self.return_flags_to_caller();
                cpu::iret(self);
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
        let saved = self.cpu.clone();
        cpu::step(self);

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
                self.cpu = saved;
                cpu::interrupt(self, 6);
                return;
            }
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
            self.cpu = saved;
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
            self.step();
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
