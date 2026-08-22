//! 起動 — ディスク・bzImage・vmlinux を機械に載せて、走り出す姿勢を作る。
//!
//! ブートローダの仕事の肩代わり (QEMUの `-kernel` と同じ役回り)。
//! ヘッダの解釈は [`crate::bzimage`] / [`crate::elf`] が持ち、ここは
//! **メモリへの配置と最初のレジスタ状態**だけを受け持つ。

pub mod bzimage;
pub mod elf;

use crate::{cpu, Disk, Machine};

/// initramfs を積んで起動するのに要るRAM量 (バイト)。
///
/// **「置ければ足りる」ではない。** カーネルは initramfs を tmpfs へ展開する
/// ので、圧縮イメージと展開後の中身がしばらく**同時にRAMに載る**。足りないと
/// カーネルは途中で展開をやめ、**それでもシェルまで来てしまう** — 「gccはある
/// のに cc1 が無い」半端なルートFSで (実際に踏んだ。ログにも何も出ない)。
///
/// gzip なら展開後の大きさは末尾4バイト (ISIZE) にそのまま書いてある。
/// 残りの 68MiB はカーネル本体・ページテーブル・スラブの取り分で、
/// **実測2点が値を挟み込んで決まった** (単位はMiB):
///
/// | イメージ | 圧縮 | 展開後 | 実測 | 効く不等式 |
/// |---|---|---|---|---|
/// | initramfs-gcc | 35.2 | 91.2 | 192で欠け、256で完全 | K > 65.6 |
/// | initramfs-lts | 19.8 | 38.5 | 128で完全 | K ≤ 69.7 |
///
/// 192MBのgccイメージで欠けたのは `collect2`・`ld`・`liblto_plugin.so`・
/// udhcpcのスクリプトだった — **`gcc -c` は通るのにリンクだけ落ち、DHCPが
/// 黙って効かない**。挟まれた幅が狭いのは、この帯を実測が本当に決めている印。
pub fn initrd_ram_needed(initrd: &[u8]) -> u64 {
    const KERNEL_WORK: u64 = 68 << 20;
    let unpacked = gzip_isize(initrd).unwrap_or(initrd.len() as u64);
    initrd.len() as u64 + unpacked + KERNEL_WORK
}

/// gzip の末尾4バイト = 展開後の大きさ (mod 2^32)。gzipでなければ None。
///
/// 連結gzip (initramfs-games / -gcc のような継ぎ足し) では**最後の塊の分**しか
/// 名乗らない。継ぎ足す側が本体なので実用上は足りるが、少なく出る向きの誤差は
/// ここにあると承知しておく
fn gzip_isize(data: &[u8]) -> Option<u64> {
    if data.len() < 18 || data[0] != 0x1F || data[1] != 0x8B {
        return None; // 非圧縮のcpioや他方式 (xz/zstd) — 大きさは名乗っていない
    }
    let n = data.len();
    Some(u32::from_le_bytes([data[n - 4], data[n - 3], data[n - 2], data[n - 1]]) as u64)
}

impl Machine {
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

    /// ISO 9660 の像から起動する (El Torito)。
    ///
    /// 実 BIOS が CD から起動するときの手順そのもの:
    ///   1. セクタ 17 の Boot Record Volume Descriptor ("CD001"、type 0、
    ///      "EL TORITO SPECIFICATION") から**ブートカタログ**の LBA を読む
    ///   2. カタログの Validation Entry (0x01 … 0xAA55) の次、Initial/Default Entry
    ///      (0x88 = 起動可) から メディア種別・ロードセグメント・セクタ数・RBA を読む
    ///   3. **no-emulation** (種別 0): ブート像を load_segment:0000 (既定 07C0) へ
    ///      count×512 バイト置き、DL=0xE0 (CD のドライブ番号) で 0000:7C00 から実行 —
    ///      isolinux はここから INT 13h AH=42 で CD を読んでカーネルを持ってくる
    ///      **floppy emulation** (種別 1-3): ISO に埋めたフロッピー像をドライブ A に見せ、
    ///      その先頭セクタから普通に起動する (既存の INT 13h 経路がそのまま使える)
    pub fn boot_from_iso(&mut self, image: Vec<u8>) -> Result<(), String> {
        const SEC: usize = 2048;
        let sector = |n: usize| -> Result<&[u8], String> {
            image
                .get(n * SEC..(n + 1) * SEC)
                .ok_or_else(|| format!("ISO が短い: セクタ {n} が無い"))
        };
        let pvd = sector(16)?;
        if &pvd[1..6] != b"CD001" {
            return Err("ISO 9660 ではない (セクタ 16 に CD001 が無い)".into());
        }
        let brvd = sector(17)?;
        if brvd[0] != 0 || &brvd[1..6] != b"CD001" || &brvd[7..30] != b"EL TORITO SPECIFICATION" {
            return Err("El Torito のブートレコードが無い (起動できない ISO)".into());
        }
        let catalog = u32::from_le_bytes(brvd[0x47..0x4B].try_into().unwrap()) as usize;
        let cat = sector(catalog)?;
        if cat[0] != 0x01 || cat[0x1E] != 0x55 || cat[0x1F] != 0xAA {
            return Err("ブートカタログの検証エントリが壊れている".into());
        }
        let e = &cat[0x20..0x40];
        if e[0] != 0x88 {
            return Err("ブートカタログの既定エントリが起動不可 (0x88 でない)".into());
        }
        let media = e[1];
        let load_seg = u16::from_le_bytes([e[2], e[3]]);
        let count = u16::from_le_bytes([e[6], e[7]]) as usize;
        let rba = u32::from_le_bytes(e[8..12].try_into().unwrap()) as usize;
        self.power_on_self_test();
        match media {
            0 => {
                // no-emulation: 像を指定セグメントへ (count は 512B 単位)
                let seg = if load_seg == 0 { 0x07C0 } else { load_seg };
                let dst = (seg as usize) << 4;
                let len = count * 512;
                let src = image
                    .get(rba * SEC..rba * SEC + len)
                    .ok_or("ブート像が ISO からはみ出している")?;
                self.mem[dst..dst + len].copy_from_slice(src);
                self.cd = Some(image);
                self.cpu.set_cs_ip(seg, 0);
                self.cpu.regs[cpu::DX] = 0x00E0; // DL = CD
            }
            1..=3 => {
                // floppy emulation: 埋め込んだフロッピー像をドライブ A に
                let size = match media {
                    1 => 1_228_800,
                    2 => 1_474_560,
                    _ => 2_949_120,
                };
                let fd = image
                    .get(rba * SEC..rba * SEC + size)
                    .ok_or("フロッピー像が ISO からはみ出している")?
                    .to_vec();
                let d = Disk::from_image(fd)?;
                let boot = d.read_sector(0).ok_or("ブートセクタが読めない")?.to_vec();
                self.disk = Some(d);
                self.cd = Some(image);
                self.mem[0x7C00..0x7E00].copy_from_slice(&boot);
                self.cpu.set_cs_ip(0x0000, 0x7C00);
                self.cpu.regs[cpu::DX] = 0x0000;
            }
            m => {
                return Err(format!(
                    "El Torito のメディア種別 {m} は未対応 (HDD エミュレーション)"
                ))
            }
        }
        Ok(())
    }

    /// テストROM (test386.asm など) を生で実行する。
    ///
    /// ROMをRAM上端の1MB境界に合わせて置き (64KBなら 0xF0000〜)、リセット直後の
    /// 姿勢 `F000:FFF0` から**BIOS HLEを通さずに**実行する。うちは普段
    /// CS==0xF000 を全部ホスト関数で肩代わりしているが、テストROMにとって
    /// そこは**実行すべき本物のコード**なので素通しにする (`bios_hle = false`)。
    ///
    /// power_on_self_test は呼ばない — IVT/BDAを撒くのはBIOSの仕事で、
    /// テストROMは自前のIDT/GDTを組む。RAMはゼロ初期化のまま渡す
    pub fn boot_rom(&mut self, rom: &[u8]) -> Result<(), String> {
        if rom.len() > 0x2_0000 {
            return Err(format!("ROMが大きすぎる ({} バイト > 128KB)", rom.len()));
        }
        let base = 0x10_0000 - rom.len();
        self.mem[base..0x10_0000].copy_from_slice(rom);
        self.bios_hle = false;
        // リセットベクタ。実CPUは 0xFFFFFFF0 (CS base 0xFFFF0000) から走り出すが、
        // ROMは1MB内の別名 (F000:FFF0) にも同じ jmp が見えるよう作られている
        self.cpu.set_cs_ip(0xF000, 0xFFF0);
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
    /// vmlinux は tools/images/extract-vmlinux.sh で bzImage から取り出せる
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

        // initrd をRAM上端寄り (1MBの余白を残してページ整列) に置く。
        // LFB を申告しているなら**その窓の下**が上端 — 重ねると、カーネルは e820 の
        // 予約域にある initrd を起動初期に複製し直してくれる (壊れはしない) が、
        // 画面に gzip の残骸がノイズとして映る
        let initrd_loc = match initrd {
            Some(data) => {
                let size = data.len() as u32;
                let top = self.lfb.map(|l| l.base).unwrap_or(self.mem.len() as u32);
                // 置き場所ではなく**展開しきれるか**で判定する。カーネルは
                // 足りなくても墜ちず、"rootfs image is not initramfs
                // (write error); looks like an initrd" と言って中身が欠けた
                // まま進む — 起動は成功して見えるので、ここで止めるしかない
                let need = initrd_ram_needed(data);
                if need > top as u64 {
                    let mb = |b: u64| b as f64 / (1 << 20) as f64;
                    return Err(format!(
                        "initrd を展開しきれない — 圧縮 {:.1}MB + 展開後 {:.1}MB \
                         + カーネル作業域 64MB = {:.0}MB 必要 (いまのRAMは {}MB)",
                        mb(size as u64),
                        mb(need - size as u64 - (64 << 20)),
                        mb(need).ceil(),
                        top >> 20,
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
        let zp = bzimage::build_zero_page(
            hdr_src,
            self.mem.len() as u64,
            CMDLINE_ADDR,
            initrd_loc,
            self.lfb,
        );
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
}
