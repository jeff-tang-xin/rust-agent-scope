interface Greeting {
  text: string;
  at: Date;
}

async function fetchGreeting(name: string): Promise<Greeting> {
  const res = await fetch(`/hello?name=${encodeURIComponent(name)}`);
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
  return { text: (await res.text()).trim(), at: new Date() };
}

function render(root: HTMLElement, g: Greeting): void {
  root.textContent = `${g.text} (${g.at.toLocaleTimeString()})`;
}

// TODO: show a spinner while loading
const root = document.getElementById("app")!;
fetchGreeting("OpenIDEA").then((g) => render(root, g), (e) => (root.textContent = String(e)));
