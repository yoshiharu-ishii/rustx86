//! bzImage を直接ロードして Linux を起動する。
//!
//! ## なぜ BIOS を通さないのか
//!
//! 実機の Linux 起動は「BIOS → ブートローダ (GRUB) → カーネルの16bit setup
//! → 32bitカーネル」と長い。だが**ブートローダの仕事はカーネルをメモリに
//! 置いて約束の状態で飛ぶこと**であり、エミュレータならそれを直接できる。
//! QEMU の `-kernel` と同じ「32bit ブートプロトコル」を使い、16bit setup を
//! 飛ばして protected-mode カーネルへ直に入る。
//!
//! ## Linux boot protocol の地図
//!
//! bzImage は先頭が「setup」で、その中に**セットアップヘッダ**が埋まっている。
//! ブートローダはここを読んでカーネルの版・エントリ・要求を知り、逆に
//! **boot_params (通称 zero page)** を組んでカーネルへ渡す。
//!
//! この段階 (3b-1) では**ヘッダを読むところまで**を、実カーネル無しで
//! テストできる形で作る。zero page の組み立てとジャンプは次段。

/// セットアップヘッダの、この実装で見るフィールド。
///
/// オフセットは Documentation/x86/boot.rst のもの。全部は見ない —
/// **32bit直接起動に要る分だけ**。他は要るようになってから足す
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupHeader {
    /// setup のセクタ数 (0 は 4 と読む、というレガシーの罠つき)。
    /// カーネル本体は「先頭 + (setup_sects+1)*512」から始まる
    pub setup_sects: u8,
    /// プロトコル版 (0x0206 なら 2.06)。0x0200 未満は古すぎて扱わない
    pub version: u16,
    /// ロード時の各種フラグ。bit0 = LOADED_HIGH (カーネルを1MBへ置く)
    pub loadflags: u8,
    /// 32bit エントリの物理アドレス。protected-mode カーネルはここから走る
    pub code32_start: u32,
    /// カーネルの「保護モード部分」のバイト数の目安 (syssize×16)
    pub syssize: u32,
}

/// setup ヘッダのマジック `HdrS` が居る場所と、各フィールドのオフセット
mod off {
    pub const SETUP_SECTS: usize = 0x1F1;
    pub const HEADER_MAGIC: usize = 0x202; // "HdrS"
    pub const VERSION: usize = 0x206;
    pub const SYSSIZE: usize = 0x1F4;
    pub const CODE32_START: usize = 0x214;
    pub const LOADFLAGS: usize = 0x211;
}

const HDRS: &[u8; 4] = b"HdrS";

impl SetupHeader {
    /// bzImage の先頭バイト列からヘッダを読む。
    ///
    /// **黙って0を返さない。** マジックが無い・版が古い・短すぎる、を
    /// それぞれ別のメッセージで断る (このリポジトリの「静かに壊れない」方針)
    pub fn parse(img: &[u8]) -> Result<SetupHeader, String> {
        if img.len() < 0x218 {
            return Err(format!(
                "bzImage が短すぎる ({} バイト、ヘッダに届かない)",
                img.len()
            ));
        }
        if &img[off::HEADER_MAGIC..off::HEADER_MAGIC + 4] != HDRS {
            return Err("セットアップヘッダのマジック 'HdrS' が無い。bzImage ではない".into());
        }
        let version = u16::from_le_bytes([img[off::VERSION], img[off::VERSION + 1]]);
        if version < 0x0200 {
            return Err(format!(
                "boot protocol {:x}.{:02x} は古すぎる (2.00 以上が要る)",
                version >> 8,
                version & 0xFF
            ));
        }
        // setup_sects=0 は「実は4」というレガシーの約束
        let setup_sects = match img[off::SETUP_SECTS] {
            0 => 4,
            n => n,
        };
        Ok(SetupHeader {
            setup_sects,
            version,
            loadflags: img[off::LOADFLAGS],
            code32_start: read32(img, off::CODE32_START),
            syssize: read32(img, off::SYSSIZE),
        })
    }

    /// protected-mode カーネル本体が bzImage の中で始まるオフセット。
    /// 先頭のブートセクタ (512) + setup のセクタ群のぶんだけ後ろ
    pub fn kernel_offset(&self) -> usize {
        (self.setup_sects as usize + 1) * 512
    }

    /// カーネルを高位 (物理1MB) に置くか (LOADED_HIGH)。
    /// bzImage は必ずこれが立つ (zImage=低位 との違い)
    pub fn loaded_high(&self) -> bool {
        self.loadflags & 0x01 != 0
    }
}

fn read32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

// ---- zero page (boot_params) を組む (3b-2) ----
//
// カーネルへ渡す構造体。ブートローダが埋め、カーネルが `%esi` 経由で読む。
// 全部で 4KB (1ページ) だが、**32bit直接起動に要る欄だけ**書く:
//   - setup ヘッダのコピー (0x1F1..) — カーネルは自分のヘッダをここで読み返す
//   - e820 メモリマップ (0x2D0..) — 使えるRAMの地図。**MachineProfileのRAMから作る**
//   - コマンドライン (別ページに置き、ポインタを 0x228 に入れる)

/// e820 の1エントリ (物理配置と同じ 20バイト)
struct E820 {
    base: u64,
    size: u64,
    kind: u32, // 1 = 使えるRAM / 2 = 予約
}

mod zp {
    pub const TYPE_OF_LOADER: usize = 0x210; // どのブートローダか (0xFF=その他)
    pub const RAMDISK_IMAGE: usize = 0x218; // initrd の物理アドレス
    pub const RAMDISK_SIZE: usize = 0x21C; // initrd のバイト数
    pub const CMDLINE_PTR: usize = 0x228;
    pub const E820_ENTRIES: usize = 0x1E8; // エントリ数 (u8)
    pub const E820_TABLE: usize = 0x2D0; // エントリの並び (各20バイト)
    pub const HDR_START: usize = 0x1F1; // setupヘッダのコピー先 (bzImage先頭0x1F1から)
    pub const HDR_END: usize = 0x268; // ヘッダの終わり (このくらい写せば足りる)

    // screen_info (先頭 0x00..0x40)。カーネル (とデコンプレッサ) は
    // **ここを見て画面に書く**。空のままだと桁数0の画面に書こうとして
    // 何も出ない — エラーメッセージすら読めなくなる (実際に困った)
    pub const ORIG_VIDEO_PAGE: usize = 0x04;
    pub const ORIG_VIDEO_MODE: usize = 0x06;
    pub const ORIG_VIDEO_COLS: usize = 0x07;
    pub const ORIG_VIDEO_LINES: usize = 0x0E;
    pub const ORIG_VIDEO_ISVGA: usize = 0x0F;
    pub const ORIG_VIDEO_POINTS: usize = 0x10;
}

/// zero page を組んで返す (4KB)。`ram_bytes` は MachineProfile の RAM。
/// `initrd` は (物理アドレス, バイト数) — カーネルはこの2欄だけで initrd を見つける
pub fn build_zero_page(
    img: &[u8],
    ram_bytes: u64,
    cmdline_ptr: u32,
    initrd: Option<(u32, u32)>,
) -> Vec<u8> {
    let mut zp = vec![0u8; 4096];

    // 1. setupヘッダをそのまま写す。カーネルは起動直後に自分のヘッダを
    //    ここから読み返すので、bzImage の 0x1F1.. をコピーしておく
    let n = (zp::HDR_END - zp::HDR_START).min(img.len().saturating_sub(zp::HDR_START));
    zp[zp::HDR_START..zp::HDR_START + n].copy_from_slice(&img[zp::HDR_START..zp::HDR_START + n]);

    // 1.5. screen_info: 80x25 のVGAテキスト画面 (mode 3) がある、と申告する。
    //      このマシンのCRTC/テキストVRAM構成と同じもの
    zp[zp::ORIG_VIDEO_PAGE] = 0;
    zp[zp::ORIG_VIDEO_MODE] = 0x03;
    zp[zp::ORIG_VIDEO_COLS] = 80;
    zp[zp::ORIG_VIDEO_LINES] = 25;
    zp[zp::ORIG_VIDEO_ISVGA] = 1;
    zp[zp::ORIG_VIDEO_POINTS] = 16;

    // 1.6. 「ブートローダが居る」と名乗る。**0のままだと、カーネルは
    //      ブートローダ不在とみなして ramdisk 欄ごと無視する** (実際に
    //      initrd が黙って捨てられた)。0xFF = 一覧に無いその他のローダ
    zp[zp::TYPE_OF_LOADER] = 0xFF;

    // 1.7. initrd の場所と大きさ。ブートローダの義務はこの2欄を埋めるだけで、
    //      中身の解釈 (cpio+gzip) は全部カーネルがやる
    if let Some((addr, size)) = initrd {
        zp[zp::RAMDISK_IMAGE..zp::RAMDISK_IMAGE + 4].copy_from_slice(&addr.to_le_bytes());
        zp[zp::RAMDISK_SIZE..zp::RAMDISK_SIZE + 4].copy_from_slice(&size.to_le_bytes());
    }

    // 2. コマンドラインのポインタ
    zp[zp::CMDLINE_PTR..zp::CMDLINE_PTR + 4].copy_from_slice(&cmdline_ptr.to_le_bytes());

    // 3. e820 メモリマップ。**RAMの地図はここでプロファイルから作る**。
    //    実機のBIOSが作る地図を、うちは MachineProfile から合成する:
    //      0x00000000 .. 0x0009FC00  使えるRAM (最初の640K、慣習的に少し欠ける)
    //      0x0009FC00 .. 0x000A0000  予約 (EBDA)
    //      0x000A0000 .. 0x00100000  予約 (VGA + BIOS ROM の窓)
    //      0x00100000 .. ram_bytes   使えるRAM (1MB以降、本体)
    let mut entries: Vec<E820> = vec![
        E820 {
            base: 0x0000_0000,
            size: 0x0009_FC00,
            kind: 1,
        },
        E820 {
            base: 0x0009_FC00,
            size: 0x0000_0400,
            kind: 2,
        },
        E820 {
            base: 0x000A_0000,
            size: 0x0006_0000,
            kind: 2,
        },
    ];
    if ram_bytes > 0x0010_0000 {
        entries.push(E820 {
            base: 0x0010_0000,
            size: ram_bytes - 0x0010_0000,
            kind: 1,
        });
    }
    zp[zp::E820_ENTRIES] = entries.len() as u8;
    for (i, e) in entries.iter().enumerate() {
        let o = zp::E820_TABLE + i * 20;
        zp[o..o + 8].copy_from_slice(&e.base.to_le_bytes());
        zp[o + 8..o + 16].copy_from_slice(&e.size.to_le_bytes());
        zp[o + 16..o + 20].copy_from_slice(&e.kind.to_le_bytes());
    }

    zp
}

/// zero page から e820 のエントリ数を読む (テスト・確認用)
pub fn zero_page_e820_count(zp: &[u8]) -> u8 {
    zp[zp::E820_ENTRIES]
}

/// zero page から i番目の e820 を読む → (base, size, kind)
pub fn zero_page_e820(zp: &[u8], i: usize) -> (u64, u64, u32) {
    let o = zp::E820_TABLE + i * 20;
    (
        u64::from_le_bytes(zp[o..o + 8].try_into().unwrap()),
        u64::from_le_bytes(zp[o + 8..o + 16].try_into().unwrap()),
        u32::from_le_bytes(zp[o + 16..o + 20].try_into().unwrap()),
    )
}
