// status: cluster-editor-graph-view-label-style
//
// Custom node-label drawing function for sigma 3.x.
//
// Sigma's stock `drawDiscNodeLabel` draws the label text directly on the
// canvas with no background; against a WebGL canvas where nodes can be
// any color and the backdrop varies with theme, that reads poorly. This
// replacement draws a rounded white background with a thin grey outline
// behind the text — the spec calls for ~6px horizontal / 2px vertical
// padding, ~3px corner radius, and a 1px solid #c8c8c8 border.
//
// Signature matches sigma's `NodeLabelDrawingFunction`; wired as
// `defaultDrawNodeLabel` in `sigmaRenderer.ts`.

import type { NodeLabelDrawingFunction } from "sigma/rendering";

const PAD_X = 6;
const PAD_Y = 2;
const RADIUS = 3;
const BORDER_COLOR = "#c8c8c8";
const FILL_COLOR = "#ffffff";

export const drawLabelWithBackground: NodeLabelDrawingFunction = (
  context,
  data,
  settings,
) => {
  if (!data.label) return;

  const size = settings.labelSize;
  const font = settings.labelFont;
  const weight = settings.labelWeight;

  context.font = `${weight} ${size}px ${font}`;
  const text = data.label;
  const metrics = context.measureText(text);
  const textWidth = metrics.width;
  const textHeight = size; // approximate; canvas text baseline metrics are imprecise

  // Anchor the label to the right of the node, vertically centered —
  // same offset shape sigma's stock label drawer uses (size + 3 to the
  // right; the y-baseline is the node center plus size/3 to look
  // optically centered with the disc).
  const left = data.x + data.size + 3;
  const baselineY = data.y + size / 3;

  // Background rect spans from the (left - PAD_X) anchor through
  // textWidth + 2*PAD_X. Vertically centered around the baseline using
  // textHeight + 2*PAD_Y as the height; the rect's top is baselineY -
  // textHeight + PAD_Y * 0 (tweaked empirically — Sigma's stock layout
  // uses `y + size/3` as the baseline so the rect needs to wrap above
  // the baseline by ~textHeight and below by a bit).
  const rectX = left - PAD_X;
  const rectY = baselineY - textHeight + (textHeight - size) - PAD_Y;
  const rectW = textWidth + PAD_X * 2;
  const rectH = textHeight + PAD_Y * 2;

  // Rounded-rect path.
  context.beginPath();
  if (typeof (context as CanvasRenderingContext2D & { roundRect?: unknown }).roundRect === "function") {
    (context as CanvasRenderingContext2D).roundRect(rectX, rectY, rectW, rectH, RADIUS);
  } else {
    // Fallback for older canvas contexts — manual arc-based rounded
    // rect. Kept narrow because sigma targets evergreen browsers; this
    // is a defensive cover.
    const r = Math.min(RADIUS, rectW / 2, rectH / 2);
    context.moveTo(rectX + r, rectY);
    context.lineTo(rectX + rectW - r, rectY);
    context.quadraticCurveTo(rectX + rectW, rectY, rectX + rectW, rectY + r);
    context.lineTo(rectX + rectW, rectY + rectH - r);
    context.quadraticCurveTo(
      rectX + rectW,
      rectY + rectH,
      rectX + rectW - r,
      rectY + rectH,
    );
    context.lineTo(rectX + r, rectY + rectH);
    context.quadraticCurveTo(rectX, rectY + rectH, rectX, rectY + rectH - r);
    context.lineTo(rectX, rectY + r);
    context.quadraticCurveTo(rectX, rectY, rectX + r, rectY);
    context.closePath();
  }

  context.fillStyle = FILL_COLOR;
  context.fill();
  context.lineWidth = 1;
  context.strokeStyle = BORDER_COLOR;
  context.stroke();

  // Label text — black to read against the white background; policy
  // color stays on the node itself, not the label.
  context.fillStyle = "#000";
  context.fillText(text, left, baselineY);
};
