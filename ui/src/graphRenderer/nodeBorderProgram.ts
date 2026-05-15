// Outlined-circle node program: same geometry/attributes as sigma's
// stock NodeCircleProgram, only the fragment shader is swapped to
// draw a ring instead of a filled disc.

import { NodeCircleProgram } from "sigma/rendering";

// Mirrors NodeCircleProgram's vertex shader (preserved so the class
// remains a drop-in replacement). `v_diffVector` is the vector from
// the centre of the node to the current fragment, `v_radius` is the
// node's radius in graph coords.
// Fragment shader: white interior + colored ring along the outer edge.
// `dist` is the depth into the circle from its outer edge — positive
// outside the circle, zero on the edge, negative toward the center.
const RING_FRAGMENT_SHADER = /* glsl */ `
precision highp float;

varying vec4 v_color;
varying vec2 v_diffVector;
varying float v_radius;

uniform float u_correctionRatio;

const vec4 transparent = vec4(0.0, 0.0, 0.0, 0.0);
const vec4 fillColor = vec4(1.0, 1.0, 1.0, 1.0);
const float borderFrac = 0.28;

void main(void) {
  float aa = u_correctionRatio * 2.0;
  float dist = length(v_diffVector) - v_radius + aa;
  float ringInner = -v_radius * borderFrac;

  #ifdef PICKING_MODE
  if (dist > aa) gl_FragColor = transparent;
  else gl_FragColor = v_color;
  #else
  if (dist > aa) {
    // Well outside the circle.
    gl_FragColor = transparent;
  } else if (dist > 0.0) {
    // Outer anti-aliased edge: fade from ring color to transparent.
    gl_FragColor = mix(v_color, transparent, dist / aa);
  } else if (dist > ringInner) {
    // Solid ring band.
    gl_FragColor = v_color;
  } else if (dist > ringInner - aa) {
    // Inner anti-aliased edge: fade from ring color to the white fill.
    gl_FragColor = mix(fillColor, v_color, (dist - (ringInner - aa)) / aa);
  } else {
    // Interior.
    gl_FragColor = fillColor;
  }
  #endif
}
`;

export class NodeBorderProgram extends NodeCircleProgram {
  getDefinition() {
    const def = super.getDefinition();
    return { ...def, FRAGMENT_SHADER_SOURCE: RING_FRAGMENT_SHADER };
  }
}
