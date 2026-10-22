// Progressive enhancement only: every page works without JavaScript.

document.addEventListener("click", (event) => {
  const button = event.target.closest("[data-copy]");
  if (button) {
    const input = button.parentElement.querySelector("input");
    input.select();
    navigator.clipboard?.writeText(input.value).then(() => {
      const label = button.textContent;
      button.textContent = "Copied";
      setTimeout(() => (button.textContent = label), 1500);
    });
    return;
  }
  // Close open dropdowns when clicking elsewhere.
  for (const open of document.querySelectorAll("details.dropdown[open]")) {
    if (!open.contains(event.target)) open.removeAttribute("open");
  }
});

document.addEventListener("focusin", (event) => {
  if (event.target.matches(".copy-field input")) event.target.select();
});

// Tree pages for large histories are sent before each file's last commit is
// known; fill those cells in once the server has worked them out.
const files = document.querySelector("table[data-last-commits]");
if (files) {
  files.classList.add("loading");
  fetch(files.dataset.lastCommits)
    .then((r) => (r.ok ? r.json() : {}))
    .catch(() => ({}))
    .then((commits) => {
      files.classList.remove("loading");
      for (const row of files.querySelectorAll("tr[data-name]")) {
        const c = commits[row.dataset.name];
        if (!c) continue;
        const link = Object.assign(document.createElement("a"), {
          href: c.url,
          className: "muted",
          textContent: c.summary,
        });
        const time = Object.assign(document.createElement("time"), {
          title: c.when_full,
          textContent: c.when,
        });
        row.querySelector(".msg").replaceChildren(link);
        row.querySelector(".when").replaceChildren(time);
      }
    });
}
