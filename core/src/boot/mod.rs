//! 起動 — ディスク・bzImage・vmlinux を機械に載せて、走り出す姿勢を作る。
//!
//! ブートローダの仕事の肩代わり (QEMUの `-kernel` と同じ役回り)。
//! ヘッダの解釈は [`crate::bzimage`] / [`crate::elf`] が持ち、ここは
//! **メモリへの配置と最初のレジスタ状態**だけを受け持つ。

pub mod bzimage;
pub mod elf;

use crate::{cpu, Disk, Machine};

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

        // --- SETUP_RNG_SEED: ブートローダの義務としてエントロピーを渡す ---
        //
        // これが無いと、RDRANDの無いこのCPUでカーネルはジッタエントロピー
        // (sched_clockを叩きながらblake2sで混ぜ続けるループ) で乱数を稼ぎ、
        // **ブート580Mのうち~100M命令**を乱数生成に燃やしていた (bootprofの実測)。
        // x86ブートプロトコルの setup_data (type 9 = SETUP_RNG_SEED) で32バイト
        // 渡せば、カーネルは起動時にCRNGを種付けして即readyになる
        // (6.x系は random.trust_bootloader が既定で有効)。
        //
        // **種は固定値** — このエミュレータの柱は決定性 (同じイメージなら
        // 命令数もビット同一) なので、ここを乱数にはできない。代償として
        // ゲストの/dev/urandomは予測可能になる。教材エミュレータとしては
        // 妥当な取引だが、秘密を扱う用途には使えない (READMEにも明記)
        const RNG_SEED_ADDR: u32 = 0x0002_1000;
        {
            let mut node = Vec::with_capacity(16 + 32);
            node.extend_from_slice(&0u64.to_le_bytes()); // next: 終端
            node.extend_from_slice(&9u32.to_le_bytes()); // type: SETUP_RNG_SEED
            node.extend_from_slice(&32u32.to_le_bytes()); // len
            node.extend_from_slice(b"rustx86 deterministic seed v1..!"); // 32B固定
            for (i, b) in node.iter().enumerate() {
                self.write_phys8(RNG_SEED_ADDR + i as u32, *b);
            }
        }

        // zero page を組んで低位に置く (慣習の 0x1_0000)
        const ZERO_PAGE_ADDR: u32 = 0x0001_0000;
        let mut zp =
            bzimage::build_zero_page(hdr_src, self.mem.len() as u64, CMDLINE_ADDR, initrd_loc);
        // setup_data (0x250, u64) を種ノードへ向ける
        zp[0x250..0x258].copy_from_slice(&(RNG_SEED_ADDR as u64).to_le_bytes());
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
