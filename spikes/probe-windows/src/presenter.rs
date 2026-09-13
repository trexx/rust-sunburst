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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

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
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, RegisterClassW,
    SW_SHOWNOACTIVATE, ShowWindow, TranslateMessage, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOPMOST,
    WS_POPUP, WS_VISIBLE,
};
use windows::core::{BOOL, Interface, w};

/// Where the window sits, and how big. Inset from (0, 0) on purpose — see above.
pub const WINDOW_X: i32 = 64;
pub const WINDOW_Y: i32 = 64;
pub const WINDOW_SIZE: i32 = 256;

/// The desktop pixel every backend watches: the window's centre.
pub const READ_X: u32 = (WINDOW_X + WINDOW_SIZE / 2) as u32;
pub const READ_Y: u32 = (WINDOW_Y + WINDOW_SIZE / 2) as u32;

/// How long a colour holds before flipping.
const FLIP_INTERVAL_MS: u64 = 100;

/// What the capture side reads, published by the presenter.
#[derive(Default)]
pub struct Signal {
    /// Increments on every colour change.
    pub flip_seq: AtomicU32,
    /// QPC taken immediately before the `Present` that carried the new colour,
    /// so a latency sample spans submit → compose → capture.
    pub flip_qpc: AtomicU64,
    /// Presents issued, for the stress comparison.
    pub presents: AtomicU64,
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
    let class = w!("sunburst_presenter");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        lpszClassName: class,
        ..Default::default()
    };
    // SAFETY: `wc` is fully initialised and `class` is a static wide string.
    // Re-registering the same class returns 0, which is fine — the class from a
    // previous run in this process is equally usable.
    unsafe { RegisterClassW(&wc) };

    // SAFETY: standard window creation with a registered class. NOACTIVATE keeps
    // it from stealing focus, TOPMOST keeps anything from covering the read
    // point, which would silently stop the signal reaching any backend.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            class,
            w!("sunburst latency"),
            WS_POPUP | WS_VISIBLE,
            WINDOW_X,
            WINDOW_Y,
            WINDOW_SIZE,
            WINDOW_SIZE,
            None,
            None,
            None,
            None,
        )
    }
    .map_err(|e| format!("CreateWindowExW: {e}"))?;

    // SAFETY: `hwnd` is live; showing without activating.
    let _ = unsafe { ShowWindow(hwnd, SW_SHOWNOACTIVATE) };
    Ok(hwnd)
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
    let built = create_window().and_then(|hwnd| create_swapchain(hwnd, mode == Mode::Stress));
    let sc = match built {
        Ok(sc) => {
            signal.tearing.store(sc.tearing, Ordering::Relaxed);
            let _ = ready.send(Ok(()));
            sc
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
        if rendered.is_err() {
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
        let _ = unsafe {
            sc.swapchain.Present(
                0,
                if sc.tearing {
                    DXGI_PRESENT_ALLOW_TEARING
                } else {
                    DXGI_PRESENT(0)
                },
            )
        };
        signal.presents.fetch_add(1, Ordering::Relaxed);
    }
}
