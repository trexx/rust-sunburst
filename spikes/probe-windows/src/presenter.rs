// SPDX-License-Identifier: GPL-2.0-or-later

//! Something to capture, with a known timestamp on it.
//!
//! # Why this exists
//!
//! CLAUDE.md budgets ~16.7ms for DWM composition — the largest line in the
//! latency table, larger than encode — and it has never been measured. Both ends
//! of that interval are timestamps we can take ourselves: an application's
//! `Present`, and a capture backend having the frame in hand. The gap between
//! them contains composition. No camera involved; a camera is only needed past
//! the decoder, for glass-to-glass.
//!
//! So this presents a known signal at a known time, and the capture backends race
//! to notice.
//!
//! # Two decisions that are not arbitrary
//!
//! **Inset from the screen edge.** The window sits at (64, 64) and the read point
//! is its centre, not the desktop origin. Windows Graphics Capture draws a
//! capture-indicator border around the captured region on some versions, and at
//! (0, 0) that border would land exactly where the probe reads — a measurement
//! that would then be timing the border, not the content.
//!
//! **Flip rarely, present constantly.** Every iteration presents; the colour
//! changes only every [`FLIP_INTERVAL_MS`]. A colour that alternates every frame
//! cannot be attributed: a refresh-capped backend seeing every third frame has no
//! way to say which `Present` it is looking at. At 100ms spacing every change is
//! unambiguous, and a ten-second run still yields ~100 samples, which is enough
//! for a p99.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};

use sunburst_core::instr::clock;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11RenderTargetView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, DXGI_FEATURE_PRESENT_ALLOW_TEARING,
    DXGI_PRESENT, DXGI_PRESENT_ALLOW_TEARING,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIFactory2, IDXGIFactory5,
    IDXGISwapChain1,
};
use windows::Win32::Foundation::RECT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetWindowRect,
    IsWindowVisible, MSG,
    PM_REMOVE, PeekMessageW, RegisterClassW, SW_SHOW, ShowWindow, TranslateMessage, WNDCLASSW,
    WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};
use windows::core::{BOOL, Interface, w};

/// Where the window sits, and how big. Inset from (0, 0) on purpose — see above.
pub const WINDOW_X: i32 = 64;
pub const WINDOW_Y: i32 = 64;
pub const WINDOW_SIZE: i32 = 256;

// Where the window is *asked* to go. Where it ends up is read back from
// `GetWindowRect` and published in `Signal` — see the note there.

/// How long a colour holds before flipping.
const FLIP_INTERVAL_MS: u64 = 100;

/// What the capture side reads, published by the presenter.
///
/// The read point is published rather than computed from the constants below.
/// The first version of this harness trusted the constants, and on a scaled
/// display the window does not land where they say: Win32 placed it in one
/// coordinate space while the capture backends read another. A transition
/// detected somewhere else on the desktop still gets timed against the most
/// recent flip, which is at most 100ms old — so it produces small,
/// plausible-looking latencies out of nothing at all.
#[derive(Default)]
pub struct Signal {
    /// Where the signal actually is, from `GetWindowRect` after creation.
    pub read_x: AtomicU32,
    pub read_y: AtomicU32,
    /// The window's real rect, for the probe to print.
    pub rect: [AtomicI32; 4],
    /// Whether Windows agrees the window is on screen.
    pub visible: AtomicBool,
    /// Increments on every colour change.
    pub flip_seq: AtomicU32,
    /// QPC taken immediately before the `Present` that carried the new colour,
    /// so a latency sample spans submit → compose → capture.
    pub flip_qpc: AtomicU64,
    /// Presents issued, for the stress comparison.
    pub presents: AtomicU64,
    /// Frames where acquiring the back buffer or its view failed. A non-zero
    /// count here means the window is never painted — which looks exactly like
    /// no window at all, because an unpainted popup shows whatever is behind it.
    pub render_errors: AtomicU64,
    /// Frames where `Present` itself failed.
    pub present_errors: AtomicU64,
    /// HRESULT of the first failure of either kind, so the cause is nameable.
    pub first_hr: AtomicI32,
    /// Set by the capture side to bring the presenter down.
    pub stop: AtomicBool,
    /// Whether tearing was available, so the stress figure can be read honestly.
    pub tearing: AtomicBool,
}

/// What the presenter is being asked to do.
///
/// The two modes want opposite things from the signal, which is why this is not
/// one knob. **Latency** needs flips far enough apart to be unambiguously
/// attributable. **Stress** needs every frame distinguishable from the last, so a
/// backend that misses frames can be seen to have missed them — and it cycles
/// through several levels rather than two, because a two-colour alternation hides
/// an even number of dropped frames.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Latency,
    Stress,
}

/// A running presenter thread.
pub struct Presenter {
    pub signal: Arc<Signal>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Presenter {
    /// Start presenting in `mode`. Stress asks for tearing, which is what lets
    /// `Present` exceed the refresh rate at all.
    pub fn start(mode: Mode) -> Result<Presenter, String> {
        let signal = Arc::new(Signal::default());
        let thread_signal = Arc::clone(&signal);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

        let thread = std::thread::Builder::new()
            .name("presenter".into())
            .spawn(move || run(&thread_signal, mode, &ready_tx))
            .map_err(|e| format!("presenter thread: {e}"))?;

        // The window and swapchain are built on that thread, so wait for it to
        // say they exist before any backend starts looking for the signal.
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Presenter {
                signal,
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("presenter thread died during setup".into()),
        }
    }
}

impl Drop for Presenter {
    fn drop(&mut self) {
        self.signal.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: the default handler, called with the parameters it was given.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

fn create_window() -> Result<HWND, String> {
    // CLAUDE.md's capture trap, and the harness is subject to it like anything
    // else: without this, window coordinates are in a different space from the
    // pixels the capture backends read, and the read point lands outside the
    // window on any scaled display.
    // SAFETY: takes a context handle by value and has no out parameters. It
    // fails only if awareness was already set, which is not an error here.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    // SAFETY: the current module's handle; passing None asks for the exe.
    let instance = unsafe { GetModuleHandleW(None) }
        .map_err(|e| format!("GetModuleHandleW: {e}"))?;

    let class = w!("sunburst_presenter");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        lpszClassName: class,
        hInstance: instance.into(),
        ..Default::default()
    };
    // SAFETY: `wc` is fully initialised and `class` is a static wide string.
    // Zero means the class is already registered from an earlier run in this
    // process, which is equally usable — anything else is a real failure and
    // CreateWindowExW below will report it.
    unsafe { RegisterClassW(&wc) };

    // SAFETY: standard window creation with a registered class. NOACTIVATE keeps
    // it from stealing focus, TOPMOST keeps anything from covering the read
    // point, which would silently stop the signal reaching any backend.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST,
            class,
            w!("sunburst latency"),
            WS_POPUP | WS_VISIBLE,
            WINDOW_X,
            WINDOW_Y,
            WINDOW_SIZE,
            WINDOW_SIZE,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
    .map_err(|e| format!("CreateWindowExW: {e}"))?;

    // SAFETY: `hwnd` is live. SW_SHOW rather than SW_SHOWNOACTIVATE: a visible
    // window is the whole point, and the first version of this produced no
    // visible window at all.
    let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };
    Ok(hwnd)
}

/// Read back where the window actually landed, and publish it.
fn publish_geometry(hwnd: HWND, signal: &Signal) {
    let mut rect = RECT::default();
    // SAFETY: `hwnd` is live and `rect` is a valid out parameter.
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok() {
        signal.rect[0].store(rect.left, Ordering::Relaxed);
        signal.rect[1].store(rect.top, Ordering::Relaxed);
        signal.rect[2].store(rect.right, Ordering::Relaxed);
        signal.rect[3].store(rect.bottom, Ordering::Relaxed);
        // The centre, so a capture-indicator border drawn at the edges cannot
        // be what gets timed.
        let x = (rect.left + (rect.right - rect.left) / 2).max(0) as u32;
        let y = (rect.top + (rect.bottom - rect.top) / 2).max(0) as u32;
        signal.read_x.store(x, Ordering::Relaxed);
        signal.read_y.store(y, Ordering::Relaxed);
    }
    // SAFETY: `hwnd` is live; returns a plain bool.
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    signal.visible.store(visible, Ordering::Relaxed);
}

struct Swapchain {
    swapchain: IDXGISwapChain1,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    tearing: bool,
}

fn create_swapchain(hwnd: HWND, uncapped: bool) -> Result<Swapchain, String> {
    // SAFETY: standard DXGI/D3D11 setup; every interface is refcounted by
    // `windows` and released on drop.
    unsafe {
        let factory: IDXGIFactory2 =
            CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).map_err(|e| format!("CreateDXGIFactory2: {e}"))?;

        // Tearing is what lets Present exceed the refresh rate. Without it the
        // stress mode measures the panel, not the paths.
        let mut tearing = BOOL(0);
        if let Ok(factory5) = factory.cast::<IDXGIFactory5>() {
            let _ = factory5.CheckFeatureSupport(
                DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                (&raw mut tearing).cast(),
                u32::try_from(size_of::<BOOL>()).expect("BOOL fits in u32"),
            );
        }
        let tearing = uncapped && tearing.as_bool();

        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            windows::Win32::Foundation::HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .map_err(|e| format!("D3D11CreateDevice: {e}"))?;
        let device = device.ok_or("D3D11CreateDevice returned no device")?;
        let context = context.ok_or("D3D11CreateDevice returned no context")?;

        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: WINDOW_SIZE as u32,
            Height: WINDOW_SIZE as u32,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: if tearing {
                DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32
            } else {
                0
            },
            ..Default::default()
        };
        let swapchain = factory
            .CreateSwapChainForHwnd(&device, hwnd, &desc, None, None)
            .map_err(|e| format!("CreateSwapChainForHwnd: {e}"))?;

        Ok(Swapchain {
            swapchain,
            device,
            context,
            tearing,
        })
    }
}

fn run(signal: &Arc<Signal>, mode: Mode, ready: &std::sync::mpsc::Sender<Result<(), String>>) {
    let built = create_window().and_then(|hwnd| {
        let sc = create_swapchain(hwnd, mode == Mode::Stress)?;
        publish_geometry(hwnd, signal);
        Ok((hwnd, sc))
    });
    let (hwnd, sc) = match built {
        Ok((hwnd, sc)) => {
            signal.tearing.store(sc.tearing, Ordering::Relaxed);
            let _ = ready.send(Ok(()));
            (hwnd, sc)
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    // Eight levels, spread wide enough that no capture format could round
    // neighbours together. The capture side compares raw bytes rather than
    // decoding a colour, so all that matters is that consecutive values differ.
    const LEVELS: usize = 8;
    let colours: [[f32; 4]; LEVELS] = std::array::from_fn(|i| {
        let v = i as f32 / (LEVELS - 1) as f32;
        [v, v, v, 1.0]
    });

    let flip_ticks = clock::ticks_per_sec() * FLIP_INTERVAL_MS / 1000;
    let mut next_flip = clock::now();
    let mut colour = 0usize;

    while !signal.stop.load(Ordering::Relaxed) {
        // Keep the window responsive without blocking the present loop.
        let mut msg = MSG::default();
        // SAFETY: `msg` is a valid out parameter; PM_REMOVE dequeues.
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            // SAFETY: `msg` was just filled by PeekMessageW.
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // Latency: change rarely, so each change is attributable. Stress: change
        // every frame, so a missed frame is visible as a missed transition.
        let now = clock::now();
        let flipping = match mode {
            Mode::Latency => now >= next_flip,
            Mode::Stress => true,
        };
        if flipping {
            colour = (colour + 1) % LEVELS;
            next_flip = now + flip_ticks;
        }

        // SAFETY: for a flip-model swapchain the back buffer rotates on Present,
        // so buffer 0 is re-fetched each frame rather than cached.
        let rendered = unsafe {
            sc.swapchain
                .GetBuffer::<ID3D11Texture2D>(0)
                .and_then(|back| {
                    let mut rtv: Option<ID3D11RenderTargetView> = None;
                    sc.device
                        .CreateRenderTargetView(&back, None, Some(&mut rtv))?;
                    if let Some(rtv) = rtv {
                        sc.context.ClearRenderTargetView(&rtv, &colours[colour]);
                    }
                    Ok(())
                })
        };
        if let Err(e) = rendered {
            // Skipping the Present is right — there is nothing to show — but the
            // first version also skipped it silently, and silently not painting
            // is indistinguishable from having no window.
            signal.render_errors.fetch_add(1, Ordering::Relaxed);
            let _ = signal
                .first_hr
                .compare_exchange(0, e.code().0, Ordering::Relaxed, Ordering::Relaxed);
            continue;
        }

        // Published before Present, so the sample spans submit → compose →
        // capture rather than starting after the frame was already handed over.
        if flipping {
            signal.flip_qpc.store(clock::now(), Ordering::Release);
            signal.flip_seq.fetch_add(1, Ordering::Release);
        }

        // SAFETY: the swapchain is live; tearing is only requested when the
        // adapter reported support and the swapchain was created with the flag.
        let presented = unsafe {
            sc.swapchain.Present(
                0,
                if sc.tearing {
                    DXGI_PRESENT_ALLOW_TEARING
                } else {
                    DXGI_PRESENT(0)
                },
            )
        };
        if presented.is_ok() {
            signal.presents.fetch_add(1, Ordering::Relaxed);
        } else {
            signal.present_errors.fetch_add(1, Ordering::Relaxed);
            let _ = signal.first_hr.compare_exchange(
                0,
                presented.0,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
    }

    // Tidy up between the two phases rather than leaving a stale window on
    // screen. Thread exit would destroy it anyway; being explicit means the
    // stress phase's window is unambiguously a new one.
    // SAFETY: `hwnd` belongs to this thread and is destroyed exactly once.
    unsafe { let _ = DestroyWindow(hwnd); }
}
