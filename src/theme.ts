// Follows the system's light or dark appearance.
const query = window.matchMedia("(prefers-color-scheme: dark)");

function apply() {
  document.documentElement.dataset.theme = query.matches ? "dark" : "light";
}

apply();
query.addEventListener("change", apply);
