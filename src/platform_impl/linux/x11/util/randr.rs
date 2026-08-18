use std::str::FromStr;
use std::{env, str};

use super::*;
use crate::dpi::validate_scale_factor;
use crate::platform_impl::platform::x11::{monitor, VideoModeHandle};

use tracing::warn;
use x11rb::protocol::randr::{self, ConnectionExt as _};

/// Complete scale authority used by Winit for one RandR output.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ScaleAuthority {
    Randr,
    Fixed(f64),
}

impl ScaleAuthority {
    pub(crate) fn scale_factor(self, pixel_size: (u32, u32), millimeter_size: (u64, u64)) -> f64 {
        match self {
            Self::Randr => calc_dpi_factor(pixel_size, millimeter_size),
            Self::Fixed(scale_factor) => scale_factor,
        }
    }

    pub(crate) fn exact_scale_factor(
        self,
        pixel_size: (u32, u32),
        millimeter_size: (u64, u64),
    ) -> Option<f64> {
        match self {
            Self::Randr => exact_randr_dpi_factor(pixel_size, millimeter_size),
            Self::Fixed(scale_factor) => {
                validate_scale_factor(scale_factor).then_some(scale_factor)
            },
        }
    }
}

pub(crate) fn resolve_scale_authority(xft_dpi: Option<f64>) -> Result<ScaleAuthority, String> {
    if env::var("WINIT_HIDPI_FACTOR").is_ok() {
        warn!(
            "The WINIT_HIDPI_FACTOR environment variable is deprecated; use WINIT_X11_SCALE_FACTOR"
        )
    }

    match env::var("WINIT_X11_SCALE_FACTOR").ok().as_deref() {
        Some(value) if value.eq_ignore_ascii_case("randr") => Ok(ScaleAuthority::Randr),
        Some("") | None => Ok(xft_dpi
            .map(|dpi| ScaleAuthority::Fixed(dpi / 96.0))
            .unwrap_or(ScaleAuthority::Randr)),
        Some(value) => {
            let scale_factor = value.parse::<f64>().map_err(|_| value.to_owned())?;
            if validate_scale_factor(scale_factor) {
                Ok(ScaleAuthority::Fixed(scale_factor))
            } else {
                Err(value.to_owned())
            }
        },
    }
}

pub fn calc_dpi_factor(pixel_size: (u32, u32), millimeter_size: (u64, u64)) -> f64 {
    // See http://xpra.org/trac/ticket/728 for more information.
    if millimeter_size.0 == 0 || millimeter_size.1 == 0 {
        warn!("XRandR reported that the display's 0mm in size, which is certifiably insane");
    }
    exact_randr_dpi_factor(pixel_size, millimeter_size).unwrap_or(1.0)
}

fn exact_randr_dpi_factor(pixel_size: (u32, u32), millimeter_size: (u64, u64)) -> Option<f64> {
    crate::platform_impl::x11::work_area::exact_randr_scale_factor(pixel_size, millimeter_size)
}

impl XConnection {
    // Retrieve DPI from Xft.dpi property
    pub fn get_xft_dpi(&self) -> Option<f64> {
        // Try to get it from XSETTINGS first.
        if let Some(xsettings_screen) = self.xsettings_screen() {
            match self.xsettings_dpi(xsettings_screen) {
                Ok(Some(dpi)) => return Some(dpi),
                Ok(None) => {},
                Err(err) => {
                    tracing::warn!("failed to fetch XSettings: {err}");
                },
            }
        }

        self.database().get_string("Xft.dpi", "").and_then(|s| f64::from_str(s).ok())
    }

    pub fn get_output_info(
        &self,
        resources: &monitor::ScreenResources,
        crtc: &randr::GetCrtcInfoReply,
    ) -> Option<(String, f64, Vec<VideoModeHandle>)> {
        let output_info = match self
            .xcb_connection()
            .randr_get_output_info(crtc.outputs[0], x11rb::CURRENT_TIME)
            .map_err(X11Error::from)
            .and_then(|r| r.reply().map_err(X11Error::from))
        {
            Ok(output_info) => output_info,
            Err(err) => {
                warn!("Failed to get output info: {:?}", err);
                return None;
            },
        };

        let bit_depth = self.default_root().root_depth;
        let output_modes = &output_info.modes;
        let resource_modes = resources.modes();

        let modes = resource_modes
            .iter()
            // XRROutputInfo contains an array of mode ids that correspond to
            // modes in the array in XRRScreenResources
            .filter(|x| output_modes.contains(&x.id))
            .map(|mode| {
                VideoModeHandle {
                    size: (mode.width.into(), mode.height.into()),
                    refresh_rate_millihertz: monitor::mode_refresh_rate_millihertz(mode)
                        .unwrap_or(0),
                    bit_depth: bit_depth as u16,
                    native_mode: mode.id,
                    // This is populated in `MonitorHandle::video_modes` as the
                    // video mode is returned to the user
                    monitor: None,
                }
            })
            .collect();

        let name = match str::from_utf8(&output_info.name) {
            Ok(name) => name.to_owned(),
            Err(err) => {
                warn!("Failed to get output name: {:?}", err);
                return None;
            },
        };
        let scale_authority = resolve_scale_authority(self.get_xft_dpi()).unwrap_or_else(|value| {
            panic!(
                "`WINIT_X11_SCALE_FACTOR` invalid; DPI factors must be either normal floats \
                 greater than 0, or `randr`. Got `{value}`"
            )
        });
        let scale_factor = scale_authority.scale_factor(
            (crtc.width.into(), crtc.height.into()),
            (output_info.mm_width.into(), output_info.mm_height.into()),
        );

        Some((name, scale_factor, modes))
    }

    pub fn set_crtc_config(
        &self,
        crtc_id: randr::Crtc,
        mode_id: randr::Mode,
    ) -> Result<(), X11Error> {
        let crtc =
            self.xcb_connection().randr_get_crtc_info(crtc_id, x11rb::CURRENT_TIME)?.reply()?;

        self.xcb_connection()
            .randr_set_crtc_config(
                crtc_id,
                crtc.timestamp,
                x11rb::CURRENT_TIME,
                crtc.x,
                crtc.y,
                mode_id,
                crtc.rotation,
                &crtc.outputs,
            )?
            .reply()
            .map(|_| ())
            .map_err(Into::into)
    }

    pub fn get_crtc_mode(&self, crtc_id: randr::Crtc) -> Result<randr::Mode, X11Error> {
        Ok(self.xcb_connection().randr_get_crtc_info(crtc_id, x11rb::CURRENT_TIME)?.reply()?.mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_fixed_scale_rejects_non_finite_authority() {
        assert_eq!(ScaleAuthority::Fixed(1.5).exact_scale_factor((1, 1), (1, 1)), Some(1.5));
        assert_eq!(ScaleAuthority::Fixed(f64::NAN).exact_scale_factor((1, 1), (1, 1)), None);
    }
}
