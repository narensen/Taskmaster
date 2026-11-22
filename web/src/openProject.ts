import { api } from "./api";
import { toast } from "./overlay";
import { setPrefs } from "./prefs";
import { queryClient } from "./query";
import { navigate } from "./router";

export function openProject(path: string, name: string) {
  void api
    .openProject(path)
    .then(() => {
      setPrefs({ lastProject: path });
      void queryClient.invalidateQueries();
      navigate("/");
      toast(`Opened ${name}`, path, 3000);
    })
    .catch((err) => toast("The project did not open", err instanceof Error ? err.message : String(err)));
}
