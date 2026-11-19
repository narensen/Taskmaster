// The checked-out branch, beside the crumbs. Nothing, when this folder is not a repository.

import { api } from "../api";
import { useLoad } from "../ui";

export function GitChip() {
  const git = useLoad(api.git, [], 8000);
  const data = git.data;
  if (!data?.repo) return null;
  const label = data.detached || !data.branch ? "detached" : data.branch;
  return (
    <span className="git-chip" data-testid="git-chip" title={data.dirty ? "Uncommitted changes" : "Checked-out branch"}>
      {label}
      {data.dirty ? " · dirty" : ""}
    </span>
  );
}
