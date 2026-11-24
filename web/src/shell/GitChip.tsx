// The checked-out branch, beside the crumbs. Nothing, when this folder is not a repository.

import { GitBranch } from "lucide-react";
import { api } from "../api";
import { useLoad } from "../ui";

export function GitChip() {
  const git = useLoad(api.git, [], 8000);
  const data = git.data;
  if (!data?.repo) return null;
  const label = data.detached || !data.branch ? "detached" : data.branch;
  return (
    <span
      className="git-chip"
      data-testid="git-chip"
      data-dirty={data.dirty ? "true" : undefined}
      title={data.dirty ? "Uncommitted changes" : "Checked-out branch"}
    >
      <GitBranch size={12} strokeWidth={1.75} className="git-chip-ico" aria-hidden />
      <span>{label}</span>
      {data.dirty ? <span className="git-chip-dirty"> · dirty</span> : ""}
    </span>
  );
}
