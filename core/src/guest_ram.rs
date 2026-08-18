//! ゲストRAMの置き場 (ADR-0026 fastmem 段1)。
//!
//! どのビルドでも `&[u8]` として同じに見える (Deref) — 変わるのは裏の
//! 置き方だけ:
//!
//! - 既定 / wasm / 非unix: `Vec<u8>` (現行と同一)
//! - unixネイティブ + `RUSTX86_FASTMEM=1`: **4GiBのPROT_NONE予約**の先頭に
//!   RAM実体 (共有バッキング) を写像する。共有バッキングなのは、第2段の
//!   線形ミラー (同じ物理ページを別の仮想番地にもう一度写す) を張るため —
//!   `Vec` (私有匿名メモリ) ではミラーが張れない
//!
//! 段1は**置き場を変えるだけ**で挙動不変 (指紋・全テストが裁定)。
//! 予約の残り (RAM末尾〜4GiB) はPROT_NONEのまま = 触るとフォルトするが、
//! 段1では誰も触らない (触る仕組みは段2のシグナル基盤と一緒に入る)。

use std::ops::{Deref, DerefMut};

/// fastmemを使うか (opt-in、既定off)。プロセスで1回だけ環境変数を読む —
/// coreが環境変数を読む唯一の箇所 (ADR-0026の互換契約: 既定は現行経路)
#[cfg(all(unix, not(target_arch = "wasm32")))]
fn fastmem_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("RUSTX86_FASTMEM")
            .map(|v| v != "0")
            .unwrap_or(false)
    })
}

pub struct GuestRam {
    inner: Inner,
}

enum Inner {
    Vec(Vec<u8>),
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    Mapped(Mapped),
}

impl GuestRam {
    /// ゼロ初期化でlenバイト確保する
    pub fn new(len: usize) -> Self {
        #[cfg(all(unix, not(target_arch = "wasm32")))]
        if fastmem_enabled() {
            match Mapped::new(len) {
                Ok(m) => {
                    return GuestRam {
                        inner: Inner::Mapped(m),
                    }
                }
                Err(e) => eprintln!("fastmem: 写像に失敗したのでVecへ退避する: {e}"),
            }
        }
        GuestRam {
            inner: Inner::Vec(vec![0; len]),
        }
    }

    /// スナップショット復元用 (RAMサイズごと差し替える)
    pub fn from_vec(v: Vec<u8>) -> Self {
        #[cfg(all(unix, not(target_arch = "wasm32")))]
        if fastmem_enabled() {
            let mut g = Self::new(v.len());
            g.copy_from_slice(&v);
            return g;
        }
        GuestRam {
            inner: Inner::Vec(v),
        }
    }

    /// 共有バッキングのfd (fastmemミラーの張り元)。Vec置き場ならNone
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    pub fn backing_fd(&self) -> Option<libc::c_int> {
        match &self.inner {
            Inner::Vec(_) => None,
            Inner::Mapped(m) => Some(m.fd),
        }
    }

    /// fastmem写像が生きているか (観測口)
    pub fn is_fastmem(&self) -> bool {
        #[cfg(all(unix, not(target_arch = "wasm32")))]
        {
            matches!(self.inner, Inner::Mapped(_))
        }
        #[cfg(not(all(unix, not(target_arch = "wasm32"))))]
        {
            false
        }
    }
}

impl Deref for GuestRam {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        match &self.inner {
            Inner::Vec(v) => v,
            #[cfg(all(unix, not(target_arch = "wasm32")))]
            Inner::Mapped(m) => unsafe { std::slice::from_raw_parts(m.base(), m.len) },
        }
    }
}

impl DerefMut for GuestRam {
    #[inline]
    fn deref_mut(&mut self) -> &mut [u8] {
        match &mut self.inner {
            Inner::Vec(v) => v,
            #[cfg(all(unix, not(target_arch = "wasm32")))]
            Inner::Mapped(m) => unsafe { std::slice::from_raw_parts_mut(m.base(), m.len) },
        }
    }
}

/// 4GiB予約 + 共有バッキングのRAM写像 (unixネイティブ専用)
#[cfg(all(unix, not(target_arch = "wasm32")))]
struct Mapped {
    /// 予約の先頭。ゲスト物理0がここに対応する
    reserve: *mut libc::c_void,
    len: usize,
    /// RAM実体のfd (Linux: memfd / macOS: 無名化済みPOSIX shm)。
    /// 第2段のミラーはこのfdをもう一度mmapして張る
    fd: libc::c_int,
}

#[cfg(all(unix, not(target_arch = "wasm32")))]
const RESERVE: usize = 4usize << 30; // ゲスト物理空間まるごと

#[cfg(all(unix, not(target_arch = "wasm32")))]
impl Mapped {
    fn new(len: usize) -> Result<Self, String> {
        unsafe {
            let reserve = libc::mmap(
                std::ptr::null_mut(),
                RESERVE,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            );
            if reserve == libc::MAP_FAILED {
                return Err("4GiB予約のmmapに失敗".into());
            }
            let fd = match Self::backing_fd(len) {
                Ok(fd) => fd,
                Err(e) => {
                    libc::munmap(reserve, RESERVE);
                    return Err(e);
                }
            };
            // RAMを予約の先頭へ実写像 (MAP_SHARED — ミラーの張り先と実体を共有)
            let p = libc::mmap(
                reserve,
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_FIXED,
                fd,
                0,
            );
            if p == libc::MAP_FAILED {
                libc::close(fd);
                libc::munmap(reserve, RESERVE);
                return Err("RAM実体のmmapに失敗".into());
            }
            Ok(Mapped { reserve, len, fd })
        }
    }

    /// 共有バッキングのfdを作る (即座に名前は消す — fdだけが命綱)
    fn backing_fd(len: usize) -> Result<libc::c_int, String> {
        unsafe {
            #[cfg(target_os = "linux")]
            let fd = {
                let name = c"rustx86-ram";
                libc::memfd_create(name.as_ptr(), 0)
            };
            #[cfg(not(target_os = "linux"))]
            let fd = {
                // macOS等: POSIX shm。名前は31文字制限があるので短く、
                // 作ったら即unlinkして無名化する
                use std::sync::atomic::{AtomicU32, Ordering};
                static CTR: AtomicU32 = AtomicU32::new(0);
                let name = format!(
                    "/rx86-{}-{}\0",
                    std::process::id(),
                    CTR.fetch_add(1, Ordering::Relaxed)
                );
                let p = name.as_ptr() as *const libc::c_char;
                let fd = libc::shm_open(p, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600);
                if fd >= 0 {
                    libc::shm_unlink(p);
                }
                fd
            };
            if fd < 0 {
                return Err("共有バッキングの作成に失敗".into());
            }
            if libc::ftruncate(fd, len as libc::off_t) != 0 {
                libc::close(fd);
                return Err("ftruncateに失敗".into());
            }
            Ok(fd)
        }
    }

    #[inline]
    fn base(&self) -> *mut u8 {
        self.reserve as *mut u8
    }
}

#[cfg(all(unix, not(target_arch = "wasm32")))]
impl Drop for Mapped {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.reserve, RESERVE);
            libc::close(self.fd);
        }
    }
}

// 写像の唯一の所有者なのでスレッド間の移動は安全 (Machineと同じ規律 —
// 実行は単一スレッド、テストハーネスがスレッドを跨いで持つことがある)
#[cfg(all(unix, not(target_arch = "wasm32")))]
unsafe impl Send for Mapped {}
#[cfg(all(unix, not(target_arch = "wasm32")))]
unsafe impl Sync for Mapped {}
