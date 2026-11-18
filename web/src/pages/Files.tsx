// The files panel as a page of its own, for when no run is open.

import { useMemo } from "react";
import { FilesPanel } from "../lazy";
import { usePath } from "../router";

export function Files() {
  usePath();
  const open = useMemo(() => {
    const params = new URLSearchParams(window.location.search);
    const path = params.get("path");
    return path ? { path, line: Number(params.get("line")) || undefined, n: Date.now() } : null;
  }, [window.location.search]);
  return (
    <div className="page files-page">
      <FilesPanel open={open} />
    </div>
  );
}
