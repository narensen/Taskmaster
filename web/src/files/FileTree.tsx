// The project's files as a tree: folders fold, only the visible rows are drawn, and the
// keyboard moves through it like a desktop tree (↑ ↓ to move, → ← to open and fold,
// Enter to open a file). Typing in the filter shows matching files as a flat list.

import { useVirtualizer } from "@tanstack/react-virtual";
import { ChevronRight, File as FileIcon, Folder, FolderOpen } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { prefetchFile } from "../query";

interface TreeNode {
  name: string;
  path: string;
  dir: boolean;
  children: TreeNode[];
}

export function buildTree(paths: string[]): TreeNode[] {
  const root: TreeNode = { name: "", path: "", dir: true, children: [] };
  const dirs = new Map<string, TreeNode>([["", root]]);
  for (const path of paths) {
    const parts = path.split("/");
    let parent = root;
    for (let i = 0; i < parts.length; i++) {
      const sub = parts.slice(0, i + 1).join("/");
      const last = i === parts.length - 1;
      if (last) {
        parent.children.push({ name: parts[i]!, path: sub, dir: false, children: [] });
      } else {
        let node = dirs.get(sub);
        if (!node) {
          node = { name: parts[i]!, path: sub, dir: true, children: [] };
          dirs.set(sub, node);
          parent.children.push(node);
        }
        parent = node;
      }
    }
  }
  const sort = (n: TreeNode) => {
    n.children.sort((a, b) => Number(b.dir) - Number(a.dir) || a.name.localeCompare(b.name));
    n.children.forEach(sort);
  };
  sort(root);
  return root.children;
}

interface Row {
  node: TreeNode;
  depth: number;
}

function flatten(nodes: TreeNode[], open: Set<string>, depth = 0, out: Row[] = []): Row[] {
  for (const node of nodes) {
    out.push({ node, depth });
    if (node.dir && open.has(node.path)) flatten(node.children, open, depth + 1, out);
  }
  return out;
}

export function FileTree({
  files,
  active,
  changed,
  filter,
  reveal,
  onOpen,
}: {
  files: string[];
  active: string | null;
  /** Files the run changed, with a one-letter mark: M, A or D. */
  changed: Map<string, string>;
  filter: string;
  /** A folder or file to unfold the tree to. */
  reveal: { path: string; n: number } | null;
  onOpen: (path: string) => void;
}) {
  const tree = useMemo(() => buildTree(files), [files]);
  const [open, setOpen] = useState<Set<string>>(() => new Set());
  const [cursor, setCursor] = useState(0);
  const scroller = useRef<HTMLDivElement>(null);
  const q = filter.trim().toLowerCase();

  const rows: Row[] = useMemo(() => {
    if (!q) return flatten(tree, open);
    return files
      .filter((f) => f.toLowerCase().includes(q))
      .slice(0, 2000)
      .map((path) => ({ node: { name: path, path, dir: false, children: [] }, depth: 0 }));
  }, [tree, open, files, q]);

  const virtual = useVirtualizer({ count: rows.length, getScrollElement: () => scroller.current, estimateSize: () => 26, overscan: 16 });
  useEffect(() => {
    const el = scroller.current;
    if (!el) return;
    const ro = new ResizeObserver(() => virtual.measure());
    ro.observe(el);
    virtual.measure();
    return () => ro.disconnect();
  }, [virtual]);

  // Unfold to a file when it opens elsewhere, and keep it in view.
  useEffect(() => {
    const target = reveal?.path ?? active;
    if (!target || q) return;
    const parts = target.split("/");
    setOpen((o) => {
      const next = new Set(o);
      for (let i = 1; i < parts.length; i++) next.add(parts.slice(0, i).join("/"));
      if (reveal && files.every((f) => f !== reveal.path)) next.add(reveal.path);
      return next.size === o.size ? o : next;
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [reveal?.n, active]);
  useEffect(() => {
    const target = reveal?.path ?? active;
    const i = rows.findIndex((r) => r.node.path === target);
    if (i >= 0) {
      setCursor(i);
      virtual.scrollToIndex(i, { align: "auto" });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [rows.length, reveal?.n, active]);

  const toggle = (path: string, to?: boolean) =>
    setOpen((o) => {
      const next = new Set(o);
      if (to ?? !next.has(path)) next.add(path);
      else next.delete(path);
      return next;
    });

  const activate = (row: Row | undefined) => {
    if (!row) return;
    if (row.node.dir) toggle(row.node.path);
    else onOpen(row.node.path);
  };

  const onKey = (e: React.KeyboardEvent) => {
    const row = rows[cursor];
    let next = cursor;
    if (e.key === "ArrowDown") next = Math.min(rows.length - 1, cursor + 1);
    else if (e.key === "ArrowUp") next = Math.max(0, cursor - 1);
    else if (e.key === "Home") next = 0;
    else if (e.key === "End") next = rows.length - 1;
    else if (e.key === "ArrowRight" && row?.node.dir) {
      if (!open.has(row.node.path)) toggle(row.node.path, true);
      else next = Math.min(rows.length - 1, cursor + 1);
    } else if (e.key === "ArrowLeft" && row) {
      if (row.node.dir && open.has(row.node.path)) toggle(row.node.path, false);
      else {
        const parent = row.node.path.split("/").slice(0, -1).join("/");
        const at = rows.findIndex((r) => r.node.path === parent);
        if (at >= 0) next = at;
      }
    } else if (e.key === "Enter" || e.key === " ") activate(row);
    else return;
    e.preventDefault();
    setCursor(next);
    virtual.scrollToIndex(next, { align: "auto" });
  };

  if (!rows.length) {
    return <div className="tree-empty">{q ? `No file matches “${filter}”.` : "No files yet."}</div>;
  }

  return (
    <div
      ref={scroller}
      className="tree"
      role="tree"
      aria-label="Project files"
      tabIndex={0}
      aria-activedescendant={rows[cursor] ? `tree-${rows[cursor]!.node.path}` : undefined}
      onKeyDown={onKey}
    >
      <div style={{ height: virtual.getTotalSize(), position: "relative" }}>
        {virtual.getVirtualItems().map((item) => {
          const row = rows[item.index]!;
          const { node } = row;
          const isOpen = node.dir && open.has(node.path);
          const mark = changed.get(node.path);
          return (
            <div
              key={node.path}
              id={`tree-${node.path}`}
              role="treeitem"
              aria-level={row.depth + 1}
              aria-expanded={node.dir ? isOpen : undefined}
              aria-selected={node.path === active}
              className={`tree-row ${node.path === active ? "on" : ""} ${item.index === cursor ? "cursor" : ""}`}
              style={{ transform: `translateY(${item.start}px)`, paddingLeft: 8 + row.depth * 12 }}
              onClick={() => {
                setCursor(item.index);
                activate(row);
              }}
              onMouseEnter={() => !node.dir && prefetchFile(node.path)}
            >
              {node.dir ? (
                <>
                  <ChevronRight size={12} className={`tree-chev ${isOpen ? "open" : ""}`} aria-hidden />
                  {isOpen ? <FolderOpen size={13} aria-hidden /> : <Folder size={13} aria-hidden />}
                </>
              ) : (
                <>
                  <span className="tree-chev-space" />
                  <FileIcon size={13} aria-hidden />
                </>
              )}
              <span className="tree-name clip">{node.name}</span>
              {mark && <span className="tree-mark">{mark}</span>}
            </div>
          );
        })}
      </div>
    </div>
  );
}
