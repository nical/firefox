/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

/// This shader renders solid colors or simple images in a color or alpha target.

#include ps_quad,sample_color0

#ifdef WR_VERTEX_SHADER

struct RepeatedPattern {
    // In layout space
    RectWithEndpoint reference_rect;
    vec2 spacing;
};

RepeatedPattern fetch_repated_pattern(int address) {
    vec4[2] payload = fetch_from_gpu_buffer_2f(address);
    return RepeatedPattern(
        RectWithEndpoint(payload[0].xy, payload[0].zw),
        payload[1].xy
    );
}

void pattern_vertex(PrimitiveInfo info) {
    RepeatedPattern pattern = fetch_repated_pattern(info.pattern_input.x);
    vec2 dst_rect_size = rect_size(info.segment.rect);
    vec2 f = (info.local_pos - info.segment.rect.p0) / dst_rect_size;


    vs_init_sample_color0(f, info.segment.uv_rect);
}

#endif

#ifdef WR_FRAGMENT_SHADER

vec4 pattern_fragment(vec4 color) {
    vec4 texel = fs_sample_color0();
    color *= texel;

    return color;
}

#if defined(SWGL_DRAW_SPAN)
// TODO!
#endif

#endif
