// A QR code for a pairing link, drawn as one SVG path. Dark modules on a light square
// with a quiet margin, in both themes, because phone cameras read that best.

import { memo, useMemo } from "react";
import { QrCode as Encoder } from "../vendor/qrcode";

export const QrCode = memo(function QrCode({ value, size = 184, label }: { value: string; size?: number; label: string }) {
  const { path, count } = useMemo(() => {
    const qr = Encoder.encodeText(value, Encoder.Ecc.MEDIUM);
    let d = "";
    for (let y = 0; y < qr.size; y++) {
      for (let x = 0; x < qr.size; x++) {
        if (qr.getModule(x, y)) d += `M${x + 4} ${y + 4}h1v1h-1z`;
      }
    }
    return { path: d, count: qr.size + 8 };
  }, [value]);
  return (
    <svg className="qr-code" width={size} height={size} viewBox={`0 0 ${count} ${count}`} role="img" aria-label={label} data-testid="qr-code" shapeRendering="crispEdges">
      <rect width={count} height={count} fill="#ffffff" />
      <path d={path} fill="#000000" />
    </svg>
  );
});
