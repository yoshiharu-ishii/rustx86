//! メモリ — 線形→物理の変換 (ページング・TLB) と、読み書きの経路。
//!
//! 読み出しは最も回数の多い経路なので分岐を足さない、書き込み側に
//! 仕掛けを寄せる (VRAM検出・自己書き換え検出)、という非対称が設計の芯。

use crate::{bus, cpu, debug, IoTarget, Machine, PageFault, TlbEntry, TLB_INVALID, TLB_SLOTS};

/// ページウォーク1回の結果。物理先頭・権限に加えて、
/// A/Dビットの書き戻し先 (表の側の物理番地) も運ぶ
struct Walk {
    base: u32,
    writable: bool,
    user_ok: bool,
    /// PDEの物理番地 (Aビットの宛先)
    pde_addr: u32,
    /// 葉 (PTE、4MBページならPDE) の物理番地 (A/Dビットの宛先)
    leaf_addr: u32,
}

impl Machine {
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
        // コードを控えたページへの書き込みは写しを無効化 (自己書き換え対策)
        self.dcache.note_write(pa);
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

    /// 線形アドレスから読む。**ページングが有効ならここで物理へ変換する**。
    /// CPUが触るのはこちら (呼び出し側は線形アドレスを渡す)
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
    /// 速度が問題になるまで足さない (「測ってから足す」— docs/reference/ci.md と同じ流儀)。
    ///
    /// こちらは**寛容な版** (デバッガ・ツール用)。未マップは RAM 外の番地を
    /// 返し、読めば 0xFF になる。CPUの実行経路は [`translate_for`] を使い、
    /// 失敗を #PF として配送する
    pub fn translate(&self, la: u32) -> u32 {
        if self.cpu.cr0 & 0x8000_0000 == 0 {
            return la;
        }
        // 覗き見はゲストを変えない: TLBにも控えず、A/Dビットも立てない。
        // 権限も見ない (デバッガはU/Sに縛られず覗けるほうが正しい)
        self.walk_page(la)
            .map(|w| w.base | (la & 0xFFF))
            .unwrap_or(0xFFFF_FFFF)
    }

    /// CPUのアクセス経路の変換。**ページ保護もここで裁く**:
    ///   - present が無ければ不在フォールト
    ///   - 書き込みで R/W=0 のページは、CR0.WP (リング0でも守る) か
    ///     リング3なら保護フォールト。カーネルはこの挙動を起動時に試験し、
    ///     #PFが来ないと「壊れたWP」として起動を拒否する (実際に拒否された)
    ///
    /// TLBヒットの直線路だけを持つ (C11: hot/cold分離)。ミス (表歩き+フィル+
    /// Aビット) と、書き込みの初回 (Dビット) は #[cold] の別関数へ —
    /// 全メモリアクセスがここを通るので、インライン展開される機械語を
    /// ヒット路の大きさに保つ
    #[inline]
    pub fn translate_for(&self, la: u32, write: bool) -> Result<u32, PageFault> {
        if self.cpu.cr0 & 0x8000_0000 == 0 {
            return Ok(la); // PG off: 線形がそのまま物理
        }
        // --- TLBを引く。当たれば表を歩かない ---
        if cfg!(feature = "opstats") {
            self.tlb_probes.set(self.tlb_probes.get() + 1);
        }
        let vpn = la >> 12;
        let slot = (vpn as usize) & (TLB_SLOTS - 1);
        let e = self.tlb[slot].get();
        if e.tag != vpn {
            if cfg!(feature = "opstats") {
                self.tlb_misses.set(self.tlb_misses.get() + 1);
            }
            return self.translate_miss(la, write, slot);
        }
        let base = e.base_flags & !0xFFF;
        // --- 権限チェック。CPLとWPは引くたびに新しく (sys_accessも) ---
        let user = self.cpu.cpl() == 3 && !self.sys_access.get();
        let wp = self.cpu.cr0 & 0x0001_0000 != 0;
        if write && e.base_flags & 1 == 0 && (user || wp) {
            return Err(PageFault {
                la,
                write,
                present: true,
            });
        }
        if user && e.base_flags & 2 == 0 {
            return Err(PageFault {
                la,
                write,
                present: true,
            });
        }
        // dirty=「Dを立てた後か」の控え — 立てた後は書き込みでも表に触らない
        if write && e.base_flags & 4 == 0 {
            return self.translate_set_dirty(la, slot, e);
        }
        Ok(base | (la & 0xFFF))
    }

    /// TLBミス側: 表を歩いて権限を裁き、通った変換だけを控えて
    /// Aビット (と初回書き込みならDビット) を帳簿に積む。
    /// A/D は実CPUがページ表へ書き戻す2ビット (Linuxはこれで「最近触った/
    /// 汚れた」を知り、test386のPOST 11はこれを検査する)。
    /// 歩く経路は &self なので直接は書けず、queue_ad → 命令境界のflush_ad
    #[cold]
    #[inline(never)]
    fn translate_miss(&self, la: u32, write: bool, slot: usize) -> Result<u32, PageFault> {
        // 不在フォールトのW/Rビットは**このアクセスの向き** — walkは向きを
        // 知らないので、エラーコードの材料はここで書き足す
        let w = self.walk_page(la).map_err(|mut f| {
            f.write = write;
            f
        })?;
        // 権限チェック (ヒット路と同じ判定を、歩いた値で)
        let user = self.cpu.cpl() == 3 && !self.sys_access.get();
        let wp = self.cpu.cr0 & 0x0001_0000 != 0;
        if write && !w.writable && (user || wp) {
            return Err(PageFault {
                la,
                write,
                present: true,
            });
        }
        if user && !w.user_ok {
            return Err(PageFault {
                la,
                write,
                present: true,
            });
        }
        // 検査を通った変換だけが控えと帳簿を書く
        self.tlb[slot].set(TlbEntry {
            tag: la >> 12,
            base_flags: w.base
                | w.writable as u32
                | ((w.user_ok as u32) << 1)
                | ((write as u32) << 2),
            leaf: w.leaf_addr,
        });
        self.queue_ad(w.pde_addr, 0x20);
        if w.leaf_addr != w.pde_addr {
            self.queue_ad(w.leaf_addr, 0x20);
        }
        if write {
            self.queue_ad(w.leaf_addr, 0x40);
        }
        Ok(w.base | (la & 0xFFF))
    }

    /// ヒットしたが D 未設定のページへの書き込み初回: 控えに dirty を立て、
    /// 表へのDビット書き戻しを帳簿に積む (ページごとに一度きりの道)
    #[cold]
    #[inline(never)]
    fn translate_set_dirty(&self, la: u32, slot: usize, e: TlbEntry) -> Result<u32, PageFault> {
        let base = e.base_flags & !0xFFF;
        self.tlb[slot].set(TlbEntry {
            tag: la >> 12,
            base_flags: e.base_flags | 4,
            leaf: e.leaf,
        });
        self.queue_ad(e.leaf, 0x40);
        Ok(base | (la & 0xFFF))
    }

    /// 2段の表を歩いて、ページの物理先頭と権限ビット、そして**表の側の番地**
    /// (Aビットを立てるPDE、Dビットを立てる葉) を返す (TLBミス時のみ)。
    /// **不在は Err(present:false)** — これは TLB に載せない (次回また歩く)
    fn walk_page(&self, la: u32) -> Result<Walk, PageFault> {
        let notp = || PageFault {
            la,
            write: false,
            present: false,
        };
        let dir = (la >> 22) & 0x3FF;
        let pde_addr = (self.cpu.cr3 & !0xFFF) + dir * 4;
        let pde = self.read_phys32(pde_addr);
        if pde & 1 == 0 {
            return Err(notp());
        }
        if pde & 0x80 != 0 {
            // 4MBページ (PSE): テーブルを引かず、ディレクトリで直に物理が決まる。
            // TLBは4K単位なので、この4Kぶんの物理先頭を作る。A/DともPDEに立つ
            let base = (pde & 0xFFC0_0000) | (la & 0x003F_F000);
            return Ok(Walk {
                base,
                writable: pde & 2 != 0,
                user_ok: pde & 4 != 0,
                pde_addr,
                leaf_addr: pde_addr,
            });
        }
        let tbl = (la >> 12) & 0x3FF;
        let pte_addr = (pde & !0xFFF) + tbl * 4;
        let pte = self.read_phys32(pte_addr);
        if pte & 1 == 0 {
            return Err(notp());
        }
        // R/W・U/S は2段の**厳しい方**が効く (両方立って初めて許す)
        Ok(Walk {
            base: pte & !0xFFF,
            writable: pde & 2 != 0 && pte & 2 != 0,
            user_ok: pde & 4 != 0 && pte & 4 != 0,
            pde_addr,
            leaf_addr: pte_addr,
        })
    }

    /// 変換失敗を記録する (最初の1件だけ)。命令の終わりで #PF になる
    pub(crate) fn note_fault(&self, f: PageFault) {
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
    /// 書き込み経路に特別な変換は無い。ここで足しているのは描画側への合図と、
    /// 自己書き換えの申告 (note_write) だけ。
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
        // コードを控えたページへの書き込みは写しを無効化 (自己書き換え対策)。
        // データページなら has_code の1判定で素通り (ADR-0007の許容コスト)
        self.dcache.note_write(a as u32);
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

    /// translate-first の速い道 (F1c-d5): 平坦セグメント・ページ内・
    /// 変換成功が全部揃うときだけ Some — **成功が確定してから実行する**ので
    /// 呼び手はguard控えを省ける。揃わなければ None (呼び手は控えてから
    /// 従来経路へ — フォールトの配送は常に従来経路 = 控えの不変条件は無傷)
    #[inline]
    pub(crate) fn fast_read32(&mut self, seg: usize, off: u32) -> Option<u32> {
        if !self.cpu.pe() || self.cpu.vm86() || !self.cpu.hidden[seg].flat_rw() {
            return None;
        }
        if off & 0xFFF > 0xFFC {
            return None; // ページ跨ぎは従来経路 (write16×2系の意味を守る)
        }
        match self.translate_for(off, false) {
            Ok(pa) => Some(self.read_phys32(pa)),
            Err(_) => None, // フォールトは従来経路が控えつきでやり直す
        }
    }

    /// fast_read32 の8bit版 (1バイトは跨げない)
    #[inline]
    pub(crate) fn fast_read8(&mut self, seg: usize, off: u32) -> Option<u8> {
        if !self.cpu.pe() || self.cpu.vm86() || !self.cpu.hidden[seg].flat_rw() {
            return None;
        }
        match self.translate_for(off, false) {
            Ok(pa) => Some(self.read_phys8(pa)),
            Err(_) => None,
        }
    }

    /// fast_read32 の16bit版
    #[inline]
    pub(crate) fn fast_read16(&mut self, seg: usize, off: u32) -> Option<u16> {
        if !self.cpu.pe() || self.cpu.vm86() || !self.cpu.hidden[seg].flat_rw() {
            return None;
        }
        if off & 0xFFF > 0xFFE {
            return None;
        }
        match self.translate_for(off, false) {
            Ok(pa) => {
                let a = pa as usize;
                if a + 2 <= self.mem.len() {
                    Some(self.mem[a] as u16 | (self.mem[a + 1] as u16) << 8)
                } else {
                    Some(0xFFFF) // read16と同じ器
                }
            }
            Err(_) => None,
        }
    }

    /// fast_read32 の書き込み版。Some(()) = 書き終えた (RAM超えの捨ても含む —
    /// write_wideと同じ意味)。None = 従来経路へ (VRAM窓・デバッガ含む)
    #[inline]
    pub(crate) fn fast_write32(&mut self, seg: usize, off: u32, val: u32) -> Option<()> {
        if !self.cpu.pe() || self.cpu.vm86() || !self.cpu.hidden[seg].flat_rw() {
            return None;
        }
        if off & 0xFFF > 0xFFC || self.dbg.on {
            return None;
        }
        let pa = match self.translate_for(off, true) {
            Ok(pa) => pa,
            Err(_) => return None,
        };
        let a = pa as usize;
        if a + 4 > self.mem.len() {
            return Some(()); // RAM超えは捨てる (write_wideと同じ完了扱い)
        }
        if a + 3 >= bus::VRAM_TEXT_BASE as usize && a <= bus::VRAM_TEXT_END as usize {
            return None; // テキストVRAM窓は遅い道 (vram_dirtyの約束)
        }
        self.mem[a..a + 4].copy_from_slice(&val.to_le_bytes());
        self.dcache.note_write(pa); // 自己書き換え: コードページなら写しを捨てる
        Some(())
    }

    /// RMW (`alu [mem], b`) の速い道: **書き込み権限で先に変換** (x86に
    /// 書き込み専用ページは無い — writable ⊆ readable) すれば、cc更新後に
    /// 失敗する道が消える。返り値は物理index (RAM内・VRAM外・ページ内)
    #[inline]
    pub(crate) fn fast_rmw32_addr(&mut self, seg: usize, off: u32) -> Option<usize> {
        if !self.cpu.pe() || self.cpu.vm86() || !self.cpu.hidden[seg].flat_rw() {
            return None;
        }
        if off & 0xFFF > 0xFFC || self.dbg.on {
            return None;
        }
        let pa = match self.translate_for(off, true) {
            Ok(pa) => pa,
            Err(_) => return None,
        };
        let a = pa as usize;
        if a + 4 > self.mem.len() {
            return None; // RAM外RMWは従来経路 (読める器0xFFの意味を守る)
        }
        if a + 3 >= bus::VRAM_TEXT_BASE as usize && a <= bus::VRAM_TEXT_END as usize {
            return None;
        }
        Some(a)
    }

    /// fast_rmw32_addr の8bit版 (跨ぎ無し)
    #[inline]
    pub(crate) fn fast_rmw8_addr(&mut self, seg: usize, off: u32) -> Option<usize> {
        if !self.cpu.pe() || self.cpu.vm86() || !self.cpu.hidden[seg].flat_rw() || self.dbg.on {
            return None;
        }
        let pa = match self.translate_for(off, true) {
            Ok(pa) => pa,
            Err(_) => return None,
        };
        let a = pa as usize;
        if a >= self.mem.len() {
            return None;
        }
        if (bus::VRAM_TEXT_BASE as usize..=bus::VRAM_TEXT_END as usize).contains(&a) {
            return None;
        }
        Some(a)
    }

    /// fast_write32 の8bit版 (write8の写し — VRAMはdirtyを立てて書く)
    #[inline]
    pub(crate) fn fast_write8(&mut self, seg: usize, off: u32, v: u8) -> Option<()> {
        if !self.cpu.pe() || self.cpu.vm86() || !self.cpu.hidden[seg].flat_rw() || self.dbg.on {
            return None;
        }
        let pa = match self.translate_for(off, true) {
            Ok(pa) => pa,
            Err(_) => return None,
        };
        let a = pa as usize;
        if a >= self.mem.len() {
            return Some(()); // write8と同じ捨て
        }
        self.mem[a] = v;
        self.dcache.note_write(pa); // 自己書き換え: コードページなら写しを捨てる
        if (bus::VRAM_TEXT_BASE as usize..=bus::VRAM_TEXT_END as usize).contains(&a) {
            self.vram_dirty = true;
        }
        Some(())
    }

    /// fast_write32 の16bit版 (VRAM窓は遅い道)
    #[inline]
    pub(crate) fn fast_write16(&mut self, seg: usize, off: u32, v: u16) -> Option<()> {
        if !self.cpu.pe() || self.cpu.vm86() || !self.cpu.hidden[seg].flat_rw() || self.dbg.on {
            return None;
        }
        if off & 0xFFF > 0xFFE {
            return None;
        }
        let pa = match self.translate_for(off, true) {
            Ok(pa) => pa,
            Err(_) => return None,
        };
        let a = pa as usize;
        if a + 2 > self.mem.len() {
            return Some(());
        }
        if a + 1 >= bus::VRAM_TEXT_BASE as usize && a <= bus::VRAM_TEXT_END as usize {
            return None;
        }
        self.mem[a..a + 2].copy_from_slice(&v.to_le_bytes());
        self.dcache.note_write(pa); // 自己書き換え: コードページなら写しを捨てる
        Some(())
    }

    /// JIT用の**記録しない**32bit読み (F1d-b、ADR-0008の脱出モデル)。
    ///
    /// [`read32`](Self::read32) との違いは1点だけ — フォールトしそうなとき
    /// `note_fault` せず None を返す。生成コードはこれを合図に**状態を1つも
    /// 変えずに脱出**し、インタプリタが同じ命令をやり直して正式に裁く
    /// (#PFの記録・配送は従来経路)。
    ///
    /// 脱出は保守的でよい (余計に脱出しても、やり直しで同じ結果になる) ので、
    /// ページ跨ぎは無条件で None に倒す。Some の道は read32 の速い道と同じ部品
    /// (translate_for + read_phys32) — 意味論を二重実装しない
    pub fn jit_try_read32(&self, addr: u32) -> Option<u32> {
        if addr & 0xFFF > 0xFFC {
            return None; // ページ跨ぎ (稀) はインタプリタに任せる
        }
        match self.translate_for(addr, false) {
            Ok(pa) => Some(self.read_phys32(pa)),
            Err(_) => None,
        }
    }

    /// JIT用の8bit読み。1バイトはページを跨げないので跨ぎ検査なし
    pub fn jit_try_read8(&self, addr: u32) -> Option<u8> {
        match self.translate_for(addr, false) {
            Ok(pa) => Some(self.read_phys8(pa)),
            Err(_) => None,
        }
    }

    /// JIT用の16bit読み。跨ぎは脱出 (インタプリタに任せる)
    pub fn jit_try_read16(&self, addr: u32) -> Option<u16> {
        if addr & 0xFFF > 0xFFE {
            return None;
        }
        match self.translate_for(addr, false) {
            Ok(pa) => Some(u16::from_le_bytes([
                self.read_phys8(pa),
                self.read_phys8(pa.wrapping_add(1)),
            ])),
            Err(_) => None,
        }
    }

    /// JIT用の32bitストア (F1d-c)。**意味論はfast_write32へ委譲** —
    /// note_write (自己書き換え検出、ADR-0020) も VRAM窓の脱出も dbg も
    /// そちらの1本に畳まれている。返り値: true=完了 / false=脱出 (何も書いてない。
    /// インタプリタが同じ命令をやり直す)。
    /// **書いた後の自ページ世代照合は生成コード側の責務** (jit.rsのn+1契約)
    pub fn jit_try_write32(&mut self, seg: usize, off: u32, val: u32) -> bool {
        self.fast_write32(seg, off, val).is_some()
    }

    /// JIT用のRMW (`alu [mem], b`)。exec.rsのfast RMW armの写し —
    /// 書き込み権限で先に変換 (writable⊆readable) するので、cc更新後に
    /// 失敗する道が無い。ALUは従来と同じ alu_w、書いたら note_write
    pub fn jit_try_rmw32(&mut self, seg: usize, off: u32, kind: u8, b: u32) -> bool {
        let Some(pa) = self.fast_rmw32_addr(seg, off) else {
            return false;
        };
        let a = u32::from_le_bytes(self.mem[pa..pa + 4].try_into().unwrap());
        let v = crate::cpu::alu::alu_w(&mut self.cpu, kind, a, b, true);
        self.mem[pa..pa + 4].copy_from_slice(&v.to_le_bytes());
        self.dcache.note_write(pa as u32);
        true
    }

    /// JIT用のpush (dcache/exec.rsのfast_push32と同一実装 — あちらが委譲してくる)。
    /// SSが平坦・32bitスタックで書き込みが確定するときだけSPを動かしてtrue
    pub fn jit_try_push32(&mut self, v: u32) -> bool {
        if !self.cpu.hidden[crate::cpu::SS].big {
            return false;
        }
        let sp = self.cpu.regs[crate::cpu::SP].wrapping_sub(4);
        if self.fast_write32(crate::cpu::SS, sp, v).is_none() {
            return false;
        }
        self.cpu.regs[crate::cpu::SP] = sp;
        true
    }

    /// JIT用のpop。読みが確定したときだけSPを確定 (push32と同じ約束)
    pub fn jit_try_pop32(&mut self) -> Option<u32> {
        if !self.cpu.hidden[crate::cpu::SS].big {
            return None;
        }
        let sp = self.cpu.regs[crate::cpu::SP];
        let v = self.fast_read32(crate::cpu::SS, sp)?;
        self.cpu.regs[crate::cpu::SP] = sp.wrapping_add(4);
        Some(v)
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
        // ページ内に収まる書き込み (呼び手が保証) なので申告は先頭1回でよい
        self.dcache.note_write(a as u32);
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
            IoTarget::Net => match &mut self.devices.net {
                // **PCI機ではISAの0x300窓は開かない。** カードはPCIスロット側に
                // 居て、番地はBARが決める — 同じ実体が両方の窓で応えると、
                // OSが2枚あると数えてしまう
                Some(net) if !self.profile.has_pci => net.read(port - bus::isa::NET_BASE),
                // カードが挿さっていなければ、ただの空きスロットである
                _ => {
                    self.unhandled_io.insert(port);
                    0xFF
                }
            },
            IoTarget::PciConfig => match &self.devices.pci {
                Some(pci) => pci.io_read(port, 1) as u8,
                None => {
                    self.unhandled_io.insert(port);
                    0xFF
                }
            },
            IoTarget::Unmapped => self.pci_io_read(port),
        }
    }

    /// PCIの窓に落ちるか。**ISAの定数`match`で名乗り手が居なかったときだけ**
    /// ここへ来る — 番地がBARで動く装置は、実行時に探すしかない
    fn pci_io_read(&mut self, port: u16) -> u8 {
        if let Some(pci) = &self.devices.pci {
            if let Some((slot, off)) = pci.io_hit(port) {
                return self.pci_slot_read(slot, off);
            }
        }
        self.unhandled_io.insert(port);
        0xFF
    }

    /// PCIの装置への読み。**挿さっている装置ごとの分岐はここ1箇所**
    fn pci_slot_read(&mut self, slot: usize, off: u16) -> u8 {
        match slot {
            // RTL8029: 皮はPCIでも中身はISA版と同じDP8390
            crate::dev::card::rtl8029::NET_SLOT => match &mut self.devices.net {
                Some(net) => net.read(off),
                None => 0xFF,
            },
            crate::dev::card::virtio_blk::BLK_SLOT => match &mut self.devices.blk {
                Some(blk) => blk.vio.read(off),
                None => 0xFF,
            },
            _ => 0xFF,
        }
    }

    /// PCIの装置への書き
    fn pci_slot_write(&mut self, slot: usize, off: u16, val: u8) {
        match slot {
            crate::dev::card::rtl8029::NET_SLOT => {
                if let Some(net) = &mut self.devices.net {
                    net.write(off, val);
                }
            }
            crate::dev::card::virtio_blk::BLK_SLOT => {
                if let Some(blk) = &mut self.devices.blk {
                    blk.vio.write(off, val);
                }
            }
            _ => {}
        }
    }

    pub fn io_write8(&mut self, port: u16, val: u8) {
        // POST診断ポート。テストROM (test386) が進行番号を書く — 足跡として残す
        if port == 0x190 {
            self.post_trail.push(val);
        }
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
            IoTarget::Net => match &mut self.devices.net {
                Some(net) if !self.profile.has_pci => net.write(port - bus::isa::NET_BASE, val),
                _ => {
                    self.unhandled_io.insert(port);
                }
            },
            IoTarget::PciConfig => match &mut self.devices.pci {
                Some(pci) => pci.io_write(port, u32::from(val), 1),
                None => {
                    self.unhandled_io.insert(port);
                }
            },
            IoTarget::Unmapped => {
                if let Some(pci) = &self.devices.pci {
                    if let Some((slot, off)) = pci.io_hit(port) {
                        self.pci_slot_write(slot, off, val);
                        return;
                    }
                }
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
}
