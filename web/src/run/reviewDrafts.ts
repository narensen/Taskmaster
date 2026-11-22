// Shared per-run diff review comments (O3):
// Line comments in both Diff and Review lenses collect as drafts, send in one batch
// (one note per owning worker with every line anchor, as one turn), and stay pinned
// after the revision with a Resolve/Reopen toggle.

import { useCallback, useEffect, useState } from "react";
import { api } from "../api";

export interface ReviewComment {
  id: string;
  file: string;
  line: number;
  text: string;
  owner: string;
  status: "draft" | "sent" | "resolved";
  createdAt: string;
}

const KEY_PREFIX = "tm:review-comments:v1:";
const EVENT_NAME = "tm-review-comments";

function read(run: string): ReviewComment[] {
  try {
    const raw = window.localStorage.getItem(KEY_PREFIX + run);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed) ? (parsed as ReviewComment[]) : [];
  } catch {
    return [];
  }
}

function write(run: string, items: ReviewComment[]) {
  try {
    window.localStorage.setItem(KEY_PREFIX + run, JSON.stringify(items));
  } catch {
    // Ignore storage quota issues.
  }
  window.dispatchEvent(new CustomEvent(EVENT_NAME, { detail: { run } }));
}

export function useReviewComments(run: string) {
  const [comments, setComments] = useState<ReviewComment[]>(() => read(run));
  const [sending, setSending] = useState(false);

  useEffect(() => {
    setComments(read(run));
    const onChange = (ev: Event) => {
      const detail = (ev as CustomEvent<{ run?: string }>).detail;
      if (!detail?.run || detail.run === run) setComments(read(run));
    };
    window.addEventListener(EVENT_NAME, onChange);
    return () => window.removeEventListener(EVENT_NAME, onChange);
  }, [run]);

  const addDraft = useCallback(
    (file: string, line: number, text: string, owner: string) => {
      const trimmed = text.trim();
      if (!trimmed) return;
      const next: ReviewComment[] = [
        ...read(run),
        {
          id: `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 7)}`,
          file,
          line,
          text: trimmed,
          owner,
          status: "draft",
          createdAt: new Date().toISOString(),
        },
      ];
      write(run, next);
      setComments(next);
    },
    [run],
  );

  const removeComment = useCallback(
    (commentId: string) => {
      const next = read(run).filter((c) => c.id !== commentId);
      write(run, next);
      setComments(next);
    },
    [run],
  );

  const toggleResolve = useCallback(
    (commentId: string) => {
      const next = read(run).map((c) =>
        c.id === commentId ? { ...c, status: c.status === "resolved" ? ("sent" as const) : ("resolved" as const) } : c,
      );
      write(run, next);
      setComments(next);
    },
    [run],
  );

  const drafts = comments.filter((c) => c.status === "draft");

  const sendDrafts = useCallback(async () => {
    const pending = read(run).filter((c) => c.status === "draft");
    if (pending.length === 0) return { count: 0, owners: 0 };
    setSending(true);
    try {
      const res = await api.addNotesBatch(run, {
        comments: pending.map((c) => ({ file: c.file, line: c.line, text: c.text, to: c.owner })),
      });
      const sentIds = new Set(pending.map((c) => c.id));
      const next = read(run).map((c) => (sentIds.has(c.id) ? { ...c, status: "sent" as const } : c));
      write(run, next);
      setComments(next);
      return { count: pending.length, owners: res.count };
    } finally {
      setSending(false);
    }
  }, [run]);

  return { comments, drafts, sending, addDraft, removeComment, toggleResolve, sendDrafts };
}
