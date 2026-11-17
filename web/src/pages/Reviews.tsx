import { useState } from "react";
import { ClipboardList, GitCompare } from "lucide-react";
import { api, DEFAULT_ROLES, type Judged } from "../api";
import { ago } from "../format";
import { Link, navigate } from "../router";
import { Badge, Code, Empty, Field, Problem, Spinner, useAction, useLoad } from "../ui";

export function Reviews() {
  const reviews = useLoad(api.reviews, [], 5000);
  const [open, setOpen] = useState(false);

  return (
    <div className="page">
      <header className="page-head">
        <h1>Reviews</h1>
        <button className="primary" onClick={() => setOpen(!open)}>
          {open ? "Close" : "New review"}
        </button>
      </header>
      <p className="muted">
        Review any repository or pull request: one harness reports problems, a different one checks each report against the code.
        Lints run first, at no token cost.
      </p>
      {open && <NewReviewForm />}
      <Problem error={reviews.error} onRetry={reviews.reload} />
      {reviews.loading && !reviews.data && <Spinner />}
      {reviews.data && reviews.data.length === 0 && !open && <Empty title="No reviews yet" icon={<ClipboardList size={18} strokeWidth={1.5} />} />}
      <div className="cards">
        {(reviews.data ?? []).map((review) => (
          <Link key={review.id} to={`/reviews/${encodeURIComponent(review.id)}`} className="card">
            <div className="row between gap">
              <span className="mono clip">{review.target}</span>
              <StatusBadge status={review.status} />
            </div>
            <div className="row gap muted small">
              <span>{ago(review.at)}</span>
              {review.confirmed !== null && (
                <span>
                  {review.confirmed} confirmed finding{review.confirmed === 1 ? "" : "s"}
                </span>
              )}
            </div>
          </Link>
        ))}
      </div>
    </div>
  );
}

function StatusBadge({ status }: { status: "running" | "done" | "failed" }) {
  return (
    <Badge tone={status === "done" ? "good" : status === "failed" ? "bad" : "busy"}>
      {status === "running" && <span className="pulse" aria-hidden />}
      {status}
    </Badge>
  );
}

function NewReviewForm() {
  const [target, setTarget] = useState("");
  const [base, setBase] = useState("");
  const [head, setHead] = useState("");
  const [pr, setPr] = useState("");
  const [dirty, setDirty] = useState(false);
  const [reviewer, setReviewer] = useState(DEFAULT_ROLES.reviewer);
  const [verifier, setVerifier] = useState(DEFAULT_ROLES.verifier);
  const action = useAction();
  const harnesses = ["claude", "agy", "grok", "opencode", "codex"];

  const submit = (event: React.FormEvent) => {
    event.preventDefault();
    void action.run("review", async () => {
      const { id } = await api.createReview({
        target: target.trim() || undefined,
        base: base.trim() || undefined,
        head: head.trim() || undefined,
        pr: pr.trim() ? Number(pr) : undefined,
        dirty,
        reviewer,
        verifier,
      });
      navigate(`/reviews/${encodeURIComponent(id)}`);
    });
  };

  return (
    <form className="card stack" onSubmit={submit}>
      <Field label="What to review" hint="A path, a repository URL or a pull request URL. Empty: this project.">
        <input value={target} onChange={(e) => setTarget(e.target.value)} placeholder="https://github.com/owner/repo/pull/12" spellCheck={false} />
      </Field>
      <div className="grid three">
        <Field label="Base">
          <input value={base} onChange={(e) => setBase(e.target.value)} placeholder="main" spellCheck={false} />
        </Field>
        <Field label="Head">
          <input value={head} onChange={(e) => setHead(e.target.value)} placeholder="HEAD" spellCheck={false} />
        </Field>
        <Field label="Pull request">
          <input value={pr} onChange={(e) => setPr(e.target.value.replace(/\D/g, ""))} inputMode="numeric" placeholder="#" />
        </Field>
      </div>
      <div className="grid two">
        <Field label="Reviewer">
          <select value={reviewer} onChange={(e) => setReviewer(e.target.value)}>
            {harnesses.map((h) => (
              <option key={h}>{h}</option>
            ))}
          </select>
        </Field>
        <Field label="Verifier">
          <select value={verifier} onChange={(e) => setVerifier(e.target.value)}>
            {harnesses.map((h) => (
              <option key={h}>{h}</option>
            ))}
          </select>
        </Field>
      </div>
      <label className="check">
        <input type="checkbox" checked={dirty} onChange={(e) => setDirty(e.target.checked)} />
        <span>Review uncommitted changes</span>
      </label>
      {reviewer === verifier && <p className="warning">The same harness checking its own report shares its blind spots.</p>}
      <Problem error={action.error} />
      <button className="primary" disabled={action.busy !== null}>
        {action.busy ? "Starting…" : "Start the review"}
      </button>
    </form>
  );
}

export function ReviewView({ id }: { id: string }) {
  const job = useLoad(() => api.review(id), [id], 4000);
  const [showRejected, setShowRejected] = useState(false);
  if (!job.data) return <div className="page">{job.error ? <Problem error={job.error} onRetry={job.reload} /> : <Spinner />}</div>;
  const j = job.data;
  const findings = j.report?.findings ?? [];
  const confirmed = findings.filter((f) => f.outcome === "confirmed");
  const rejected = findings.filter((f) => f.outcome === "rejected");

  return (
    <div className="page">
      <nav className="crumbs">
        <Link to="/reviews">Reviews</Link> / <span className="mono">{j.id}</span>
      </nav>
      <header className="run-head">
        <div className="row between wrap gap">
          <h1 className="mono clip">{j.target}</h1>
          <StatusBadge status={j.status} />
        </div>
        <div className="row wrap gap muted small">
          <span>
            {j.reviewer} reviewed, {j.verifier} checked
          </span>
          <span>{ago(j.at)}</span>
          {j.report && (
            <span className="mono">
              {j.report.base.slice(0, 10)}..{j.report.dirty ? "working tree" : j.report.head.slice(0, 10)}
            </span>
          )}
        </div>
      </header>
      {j.status === "running" && <Spinner label="Reviewing. This takes a few minutes." />}
      {j.error && <p className="error-line">{j.error}</p>}
      {j.report && !j.report.reviewed && <Empty title="Nothing changed, so nothing was reviewed" icon={<GitCompare size={18} strokeWidth={1.5} />} />}
      {j.report?.reviewed && (
        <>
          <h2 className="section-title">
            {confirmed.length ? `${confirmed.length} confirmed finding${confirmed.length === 1 ? "" : "s"}` : "No finding held up"}
          </h2>
          <div className="list">
            {confirmed.map((finding, i) => (
              <FindingItem key={i} finding={finding} />
            ))}
          </div>
          {rejected.length > 0 && (
            <button className="link" onClick={() => setShowRejected(!showRejected)}>
              {showRejected ? "Hide" : "Show"} {rejected.length} finding{rejected.length === 1 ? "" : "s"} the verifier disproved
            </button>
          )}
          {showRejected && (
            <div className="list">
              {rejected.map((finding, i) => (
                <FindingItem key={i} finding={finding} />
              ))}
            </div>
          )}
          {(j.report.dropped > 0 || j.report.duplicates > 0) && (
            <p className="muted small">
              {j.report.dropped} vague report{j.report.dropped === 1 ? "" : "s"} dropped, {j.report.duplicates} duplicate
              {j.report.duplicates === 1 ? "" : "s"}.
            </p>
          )}
        </>
      )}
      {j.text && (
        <details>
          <summary className="muted">The report as text</summary>
          <Code>{j.text}</Code>
        </details>
      )}
    </div>
  );
}

function FindingItem({ finding }: { finding: Judged }) {
  return (
    <div className={`item finding ${finding.outcome}`}>
      <div className="mono small">
        {finding.file}:{finding.line}
      </div>
      <p>{finding.failure}</p>
      {finding.why && <p className="muted small">Disproved: {finding.why}</p>}
    </div>
  );
}
