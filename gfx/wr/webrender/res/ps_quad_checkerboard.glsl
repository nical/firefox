/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#include ps_quad

flat varying mediump vec4 v_color1;
flat varying mediump vec4 v_color2;
flat varying highp vec2 v_cell_size;

#ifdef WR_VERTEX_SHADER

void pattern_vertex(PrimitiveInfo info) {
    int address = info.pattern_input.x;
    vec4[3] data = fetch_from_gpu_buffer_3f(address);

    v_color1 = data[0];
    v_color2 = data[1];
    v_cell_size = data[2].xy;
}

#endif

#ifdef WR_FRAGMENT_SHADER

vec4 pattern_fragment(vec4 color) {
    vec2 cell_coords = vLocalPos / v_cell_size;
    float checker = mod(floor(cell_coords.x) + floor(cell_coords.y), 2.0);
    vec4 pattern_color = mix(v_color1, v_color2, checker);
    return color * pattern_color;
}

#endif
