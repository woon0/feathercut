//! A desktop-only compositor layer, underneath the application's opaque content.
//! No desktop pixels are read back into the application.
#![allow(non_snake_case)]
use std::cell::{Cell, RefCell};
use windows::{
    Foundation::{IPropertyValue, PropertyValue},
    Graphics::Effects::{
        IGraphicsEffect, IGraphicsEffect_Impl, IGraphicsEffectSource, IGraphicsEffectSource_Impl,
    },
    System::{DispatcherQueue, DispatcherQueueController},
    UI::Composition::{
        CompositionEffectBrush, CompositionEffectSourceParameter, Compositor,
        Desktop::DesktopWindowTarget, SpriteVisual,
    },
    Win32::{
        Foundation::{E_INVALIDARG, E_POINTER, HWND},
        Graphics::Direct2D::CLSID_D2D1GaussianBlur,
        System::WinRT::{
            Composition::ICompositorDesktopInterop,
            CreateDispatcherQueueController, DQTAT_COM_STA, DQTYPE_THREAD_CURRENT,
            DispatcherQueueOptions,
            Graphics::Direct2D::{
                GRAPHICS_EFFECT_PROPERTY_MAPPING, GRAPHICS_EFFECT_PROPERTY_MAPPING_DIRECT,
                IGraphicsEffectD2D1Interop, IGraphicsEffectD2D1Interop_Impl,
            },
            RO_INIT_SINGLETHREADED, RoInitialize, RoUninitialize,
        },
    },
    core::{GUID, HSTRING, Interface, PCWSTR, Result, implement},
};

#[implement(IGraphicsEffect, IGraphicsEffectSource, IGraphicsEffectD2D1Interop)]
struct Gaussian {
    name: RefCell<HSTRING>,
    source: IGraphicsEffectSource,
}
impl IGraphicsEffectSource_Impl for Gaussian_Impl {}
impl IGraphicsEffect_Impl for Gaussian_Impl {
    fn Name(&self) -> Result<HSTRING> {
        Ok(self.name.borrow().clone())
    }
    fn SetName(&self, name: &HSTRING) -> Result<()> {
        *self.name.borrow_mut() = name.clone();
        Ok(())
    }
}
impl IGraphicsEffectD2D1Interop_Impl for Gaussian_Impl {
    fn GetEffectId(&self) -> Result<GUID> {
        Ok(CLSID_D2D1GaussianBlur)
    }
    fn GetNamedPropertyMapping(
        &self,
        name: &PCWSTR,
        index: *mut u32,
        mapping: *mut GRAPHICS_EFFECT_PROPERTY_MAPPING,
    ) -> Result<()> {
        if index.is_null() || mapping.is_null() || name.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: these are non-null output pointers supplied by the compositor.
        unsafe {
            if name.to_string()? != "Amount" {
                return Err(E_INVALIDARG.into());
            }
            index.write(0);
            mapping.write(GRAPHICS_EFFECT_PROPERTY_MAPPING_DIRECT);
        }
        Ok(())
    }
    fn GetPropertyCount(&self) -> Result<u32> {
        Ok(3)
    }
    fn GetProperty(&self, index: u32) -> Result<IPropertyValue> {
        // Standard deviation, balanced optimization, hard border mode.
        match index {
            0 => PropertyValue::CreateSingle(0.0)?.cast(),
            1 => PropertyValue::CreateUInt32(1)?.cast(),
            2 => PropertyValue::CreateUInt32(1)?.cast(),
            _ => Err(E_INVALIDARG.into()),
        }
    }
    fn GetSource(&self, index: u32) -> Result<IGraphicsEffectSource> {
        if index == 0 {
            Ok(self.source.clone())
        } else {
            Err(E_INVALIDARG.into())
        }
    }
    fn GetSourceCount(&self) -> Result<u32> {
        Ok(1)
    }
}

fn gaussian_brush(compositor: &Compositor) -> Result<CompositionEffectBrush> {
    let source = CompositionEffectSourceParameter::Create(&HSTRING::from("desktop"))?;
    let effect: IGraphicsEffect = Gaussian {
        name: RefCell::new("Blur".into()),
        source: source.cast()?,
    }
    .into();
    let properties =
        windows_collections::IIterable::<HSTRING>::from(vec![HSTRING::from("Blur.Amount")]);
    let factory = compositor.CreateEffectFactoryWithProperties(&effect, &properties)?;
    let brush = factory.CreateBrush()?;
    brush.SetSourceParameter(
        &HSTRING::from("desktop"),
        // HostBackdrop supplies Windows' pre-blurred material: even a near-zero
        // Gaussian then starts from a heavily blurred image. This clear,
        // non-redirected host instead needs the raw pixels behind its visual.
        &compositor.CreateBackdropBrush()?,
    )?;
    Ok(brush)
}

fn set_gaussian_radius(
    brush: &CompositionEffectBrush,
    visual: &SpriteVisual,
    amount: f32,
) -> Result<()> {
    let fraction = amount.clamp(0.0, 100.0) / 100.0;
    brush
        .Properties()?
        // Gentle at the bottom; enough spread to remove fine detail at the top.
        // 1%=0.004px, 20%=1.6px, 50%=10px, 100%=40px standard deviation.
        .InsertScalar(&HSTRING::from("Blur.Amount"), fraction * fraction * 40.0)?;
    // Never mix sharp background details back into the Gaussian result.
    // Glass tint is controlled separately by the UI transparency setting.
    visual.SetOpacity(1.0)
}

struct BlurLayer {
    hwnd: isize,
    target: DesktopWindowTarget,
    visual: SpriteVisual,
    brush: CompositionEffectBrush,
    _compositor: Compositor,
    _queue: Option<DispatcherQueueController>,
    underlay: super::underlay::Underlay,
    _apartment: Apartment,
}
struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
        }
    }
}
impl BlurLayer {
    fn new(hwnd: HWND) -> Result<Self> {
        // SAFETY: composition is created and retained on the Slint event-loop thread.
        unsafe {
            RoInitialize(RO_INIT_SINGLETHREADED)?;
        }
        let apartment = Apartment;
        let queue = if DispatcherQueue::GetForCurrentThread().is_ok() {
            None
        } else {
            Some(unsafe {
                CreateDispatcherQueueController(DispatcherQueueOptions {
                    dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
                    threadType: DQTYPE_THREAD_CURRENT,
                    apartmentType: DQTAT_COM_STA,
                })?
            })
        };
        let compositor = Compositor::new()?;
        let interop: ICompositorDesktopInterop = compositor.cast()?;
        let underlay = super::underlay::Underlay::new(hwnd)?;
        // DirectComposition is ABOVE the target HWND's swapchain even when false.
        // Target a separate window below Feathercut so no app pixels enter this effect.
        let target = unsafe { interop.CreateDesktopWindowTarget(underlay.hwnd, false)? };
        let brush = gaussian_brush(&compositor)?;
        let visual = compositor.CreateSpriteVisual()?;
        visual.SetBrush(&brush)?;
        visual.SetRelativeSizeAdjustment(windows_numerics::Vector2 { X: 1.0, Y: 1.0 })?;
        target.SetRoot(&visual)?;
        Ok(Self {
            hwnd: hwnd.0 as isize,
            target,
            visual,
            brush,
            _compositor: compositor,
            _queue: queue,
            underlay,
            _apartment: apartment,
        })
    }
    fn set_amount(&mut self, transparency: f32, amount: f32) -> Result<()> {
        set_gaussian_radius(&self.brush, &self.visual, amount)?;
        self.underlay
            .set_enabled(transparency > 0.0 && amount > 0.0);
        Ok(())
    }
}
impl Drop for BlurLayer {
    fn drop(&mut self) {
        let _ = self.target.Close();
        let _ = self._compositor.Close();
        if let Some(queue) = &self._queue {
            let _ = queue.ShutdownQueueAsync();
        }
    }
}
thread_local! {
    static LAYER: RefCell<Option<BlurLayer>> = const { RefCell::new(None) };
    static FAILED_HWND: Cell<isize> = const { Cell::new(0) };
}
pub fn apply(hwnd: HWND, transparency: f32, amount: f32, width: u32, height: u32) -> bool {
    if FAILED_HWND.with(|failed| failed.get() == hwnd.0 as isize) {
        return false;
    }
    LAYER.with(|state| {
        let mut layer = state.borrow_mut();
        if layer
            .as_ref()
            .is_some_and(|layer| layer.hwnd != hwnd.0 as isize)
        {
            *layer = None;
        }
        if layer.is_none() {
            if amount <= 0.0 || transparency <= 0.0 {
                return false;
            }
            let Ok(created) = BlurLayer::new(hwnd) else {
                FAILED_HWND.with(|failed| failed.set(hwnd.0 as isize));
                return false;
            };
            *layer = Some(created);
        }
        if let Some(layer) = &mut *layer {
            let _ = layer.visual.SetSize(windows_numerics::Vector2 {
                X: width as f32,
                Y: height as f32,
            });
            if layer.set_amount(transparency, amount).is_err() {
                layer.underlay.set_enabled(false);
                return false;
            }
        }
        amount > 0.0 && transparency > 0.0
    })
}
pub fn resize(width: u32, height: u32) {
    LAYER.with(|state| {
        if let Some(layer) = &*state.borrow() {
            let _ = layer.visual.SetSize(windows_numerics::Vector2 {
                X: width as f32,
                Y: height as f32,
            });
        }
    });
}
pub fn shutdown() {
    LAYER.with(|state| *state.borrow_mut() = None);
}
pub fn sync() {
    // Native callbacks may occur while creating/dropping a layer. Skip reentry.
    LAYER.with(|state| {
        if let Ok(layer) = state.try_borrow()
            && let Some(layer) = &*layer
        {
            layer.underlay.sync();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unattached_gaussian_effect_changes_radius_without_fading() -> Result<()> {
        // Exercise the actual Windows effect factory without any HWND/desktop
        // target: no window is opened, no desktop pixels are sampled or captured.
        unsafe {
            RoInitialize(RO_INIT_SINGLETHREADED)?;
        }
        let _apartment = Apartment;
        let queue = unsafe {
            CreateDispatcherQueueController(DispatcherQueueOptions {
                dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
                threadType: DQTYPE_THREAD_CURRENT,
                apartmentType: DQTAT_COM_STA,
            })?
        };
        let compositor = Compositor::new()?;
        let result = (|| -> Result<()> {
            let brush = gaussian_brush(&compositor)?;
            let visual = compositor.CreateSpriteVisual()?;
            visual.SetBrush(&brush)?;
            for (amount, radius) in [
                (0.0, 0.0),
                (1.0, 0.004),
                (20.0, 1.6),
                (25.0, 2.5),
                (50.0, 10.0),
                (100.0, 40.0),
            ] {
                set_gaussian_radius(&brush, &visual, amount)?;
                let mut actual = 0.0;
                brush
                    .Properties()?
                    .TryGetScalar(&HSTRING::from("Blur.Amount"), &mut actual)?;
                assert!(
                    (actual - radius).abs() < 0.00001,
                    "Windows must receive the requested Gaussian radius"
                );
                assert_eq!(
                    visual.Opacity()?,
                    1.0,
                    "Blur must never act as a fade slider"
                );
            }
            Ok(())
        })();
        compositor.Close()?;
        let _ = queue.ShutdownQueueAsync();
        result
    }
}
