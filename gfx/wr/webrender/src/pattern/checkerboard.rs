/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use api::units::*;
use api::ColorF;
use crate::pattern::{Pattern, PatternKind, PatternShaderInput, PatternTextureInput};
use crate::pattern::{PatternBuilder, PatternBuilderContext, PatternBuilderState};
use crate::renderer::GpuBufferBuilder;

pub fn checkerboard_pattern(
    color1: ColorF,
    color2: ColorF,
    cell_size: LayoutSize,
    gpu_buffer_builder: &mut GpuBufferBuilder
) -> Pattern {
    let mut writer = gpu_buffer_builder.f32.write_blocks(3);

    let premul_color1 = color1.premultiplied();
    let premul_color2 = color2.premultiplied();

    writer.push_one([
        premul_color1.r,
        premul_color1.g,
        premul_color1.b,
        premul_color1.a,
    ]);
    writer.push_one([
        premul_color2.r,
        premul_color2.g,
        premul_color2.b,
        premul_color2.a,
    ]);
    writer.push_one([
        cell_size.width,
        cell_size.height,
        0.0,
        0.0,
    ]);

    let address = writer.finish();

    let is_opaque = color1.a >= 1.0 && color2.a >= 1.0;

    Pattern {
        kind: PatternKind::Checkerboard,
        shader_input: PatternShaderInput(
            address.as_int(),
            0,
        ),
        texture_input: PatternTextureInput::default(),
        base_color: ColorF::WHITE,
        is_opaque,
    }
}

#[cfg_attr(feature = "capture", derive(Serialize))]
#[cfg_attr(feature = "replay", derive(Deserialize))]
#[derive(Debug, MallocSizeOf)]
pub struct CheckerboardPattern {
    pub color1: ColorF,
    pub color2: ColorF,
    pub cell_size: LayoutSize,
}

impl PatternBuilder for CheckerboardPattern {
    fn build(
        &self,
        _sub_rect: Option<DeviceRect>,
        _ctx: &PatternBuilderContext,
        state: &mut PatternBuilderState,
    ) -> Pattern {
        checkerboard_pattern(
            self.color1,
            self.color2,
            self.cell_size,
            state.frame_gpu_data,
        )
    }

    fn get_base_color(
        &self,
        _ctx: &PatternBuilderContext,
    ) -> ColorF {
        ColorF::WHITE
    }

    fn use_shared_pattern(&self) -> bool { true }
}

