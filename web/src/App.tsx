import { useEffect, useState } from "react";
import { setToken, token, tokenFromRedirect } from "./api";
import { Link, match, navigate, usePath } from "./router";
import { Dictionary } from "./pages/Dictionary";
import { Errorography } from "./pages/Errorography";
import { NewRun } from "./pages/NewRun";
import { ReviewView, Reviews } from "./pages/Reviews";
import { RunView } from "./pages/RunView";
import { Runs } from "./pages/Runs";
import { Settings } from "./pages/Settings";
import { SignIn } from "./pages/SignIn";

const NAV = [
  { to: "/", label: "Runs", icon: "M4 6h16M4 12h16M4 18h10", active: (p: string) => p === "/" || p.startsWith("/runs") },
  { to: "/dictionary", label: "Dictionary", icon: "M5 4h11a3 3 0 0 1 3 3v13H8a3 3 0 0 1-3-3zM5 17a3 3 0 0 1 3-3h11" },
  { to: "/reviews", label: "Reviews", icon: "M9 12l2 2 4-4M12 3l8 4v5c0 5-3.5 8-8 9-4.5-1-8-4-8-9V7z" },
  { to: "/errorography", label: "Mistakes", icon: "M12 9v4M12 17h.01M10.3 4l-8 14a2 2 0 0 0 1.7 3h16a2 2 0 0 0 1.7-3l-8-14a2 2 0 0 0-3.4 0z" },
  { to: "/settings", label: "Settings", icon: "M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM19 12a7 7 0 0 0-.1-1.2l2-1.6-2-3.4-2.4 1a7 7 0 0 0-2-1.2L14 3h-4l-.5 2.6a7 7 0 0 0-2 1.2l-2.4-1-2 3.4 2 1.6A7 7 0 0 0 5 12c0 .4 0 .8.1 1.2l-2 1.6 2 3.4 2.4-1a7 7 0 0 0 2 1.2L10 21h4l.5-2.6a7 7 0 0 0 2-1.2l2.4 1 2-3.4-2-1.6c.1-.4.1-.8.1-1.2z" },
];

function useSignedIn(): boolean {
  const [signedIn, setSignedIn] = useState(() => {
    // Back from an account sign-in: keep the token and drop it from the address bar.
    const handed = tokenFromRedirect();
    if (handed) {
      setToken(handed);
      const back = window.location.pathname === "/signed-in" ? "/" : window.location.pathname + window.location.search;
      window.history.replaceState(null, "", back);
    }
    return token() !== null;
  });
  useEffect(() => {
    const update = () => setSignedIn(token() !== null);
    window.addEventListener("taskmaster-auth", update);
    window.addEventListener("storage", update);
    return () => {
      window.removeEventListener("taskmaster-auth", update);
      window.removeEventListener("storage", update);
    };
  }, []);
  return signedIn;
}

function Page({ path }: { path: string }) {
  let params: Record<string, string> | null;
  if (path === "/" || path === "/runs") return <Runs />;
  if (path === "/runs/new") return <NewRun />;
  if ((params = match(path, "/runs/:id"))) return <RunView id={params.id!} />;
  if (path === "/dictionary") return <Dictionary />;
  if (path === "/errorography") return <Errorography />;
  if (path === "/reviews") return <Reviews />;
  if ((params = match(path, "/reviews/:id"))) return <ReviewView id={params.id!} />;
  if (path === "/settings") return <Settings />;
  return (
    <div className="page">
      <h1>Nothing here</h1>
      <p>
        <Link to="/">Back to runs</Link>
      </p>
    </div>
  );
}

export function App() {
  const path = usePath();
  const signedIn = useSignedIn();

  useEffect(() => {
    if (signedIn && path === "/signed-in") navigate("/", true);
  }, [signedIn, path]);

  if (!signedIn) return <SignIn />;

  return (
    <div className="shell">
      <nav className="nav" aria-label="Main">
        <Link to="/" className="brand">
          <img src="/icon.png" alt="" width={28} height={28} />
          <span>Taskmaster</span>
        </Link>
        <div className="nav-items">
          {NAV.map((item) => {
            const on = item.active ? item.active(path) : path.startsWith(item.to);
            return (
              <Link key={item.to} to={item.to} className={on ? "on" : ""} aria-current={on ? "page" : undefined}>
                <svg viewBox="0 0 24 24" width="22" height="22" aria-hidden>
                  <path d={item.icon} />
                </svg>
                <span>{item.label}</span>
              </Link>
            );
          })}
        </div>
      </nav>
      <main className="main">
        <Page path={path} />
      </main>
    </div>
  );
}
