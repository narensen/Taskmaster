import { CircleCheck } from "lucide-react";
import { api, PROMOTE_AT } from "../api";
import { Badge, Code, Empty, Problem, Spinner, useLoad } from "../ui";

export function Errorography() {
  const book = useLoad(api.errorography, []);
  const models = Object.entries(book.data?.models ?? {}).filter(([, list]) => list.length > 0);

  return (
    <div className="page">
      <header className="page-head">
        <h1>Mistakes</h1>
      </header>
      <p className="muted">
        The Errorography: the failures each model keeps repeating, and what fixed them. A mistake seen {PROMOTE_AT} times is put in
        front of that model before its next page, and only that model.
      </p>
      <Problem error={book.error} onRetry={book.reload} />
      {book.loading && !book.data && <Spinner />}
      {book.data && models.length === 0 && (
        <Empty title="No mistakes recorded" icon={<CircleCheck size={18} strokeWidth={1.5} />}>
          Failed checks are recorded here as runs go.
        </Empty>
      )}
      {models.map(([model, list]) => (
        <section key={model}>
          <h2 className="section-title mono">{model}</h2>
          <div className="list">
            {[...list]
              .sort((a, b) => b.count - a.count)
              .map((mistake) => (
                <details key={mistake.signature} className="item">
                  <summary>
                    <div className="row between gap">
                      <span className="mono small clip">{mistake.signature}</span>
                      <div className="row gap small keep">
                        {mistake.count >= PROMOTE_AT && <Badge tone="ask">warned</Badge>}
                        <span className="count-pill">{mistake.count}×</span>
                      </div>
                    </div>
                  </summary>
                  <div className="stack small">
                    <span className="muted">Last seen as</span>
                    <Code>{mistake.sample}</Code>
                    {mistake.fix && (
                      <>
                        <span className="muted">What fixed it</span>
                        <Code>{mistake.fix}</Code>
                      </>
                    )}
                  </div>
                </details>
              ))}
          </div>
        </section>
      ))}
    </div>
  );
}
