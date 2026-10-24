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

// Inline review comments: open the comment form below a diff line in place
// instead of reloading the page (the link works without JavaScript too).
const inlineTemplate = document.getElementById("inline-form-template");
document.addEventListener("click", (event) => {
  const add = event.target.closest(".add-comment");
  if (add && inlineTemplate) {
    event.preventDefault();
    const key = `${add.dataset.side}:${add.dataset.line}:${add.dataset.path}`;
    let row = add.closest("tr");
    // Below the line's existing threads, unless a form is already open.
    while (row.nextElementSibling?.classList.contains("thread-row")) {
      row = row.nextElementSibling;
      const open = row.querySelector(".inline-form");
      if (open?.dataset.for === key) return open.elements.body.focus();
    }
    const form = inlineTemplate.content.firstElementChild.cloneNode(true);
    form.dataset.for = key;
    for (const key of ["path", "side", "line"]) form.elements[key].value = add.dataset[key];
    if (add.dataset.side === "new") {
      // The line's text, without the "+" button.
      const code = add.closest("td").cloneNode(true);
      code.querySelector(".add-comment").remove();
      form.querySelector("[data-suggest]").dataset.text = code.textContent;
    }
    revealSuggest(form);
    const cell = Object.assign(document.createElement("td"), { colSpan: 3 });
    const tr = Object.assign(document.createElement("tr"), { className: "thread-row" });
    cell.append(form);
    tr.append(cell);
    row.after(tr);
    form.elements.body.focus();
    return;
  }
  const cancel = event.target.closest(".inline-form [data-cancel]");
  if (cancel) return cancel.closest("tr").remove();
  // "Suggest a change": start a suggestion block with the line's text.
  const suggest = event.target.closest("[data-suggest]");
  if (suggest) {
    const body = suggest.closest("form").elements.body;
    const block = "```suggestion\n" + suggest.dataset.text + "\n```\n";
    body.value = body.value ? `${body.value.trimEnd()}\n\n${block}` : block;
    body.focus();
    // Select the suggested line so it can be typed over.
    const start = body.value.lastIndexOf(block) + "```suggestion\n".length;
    body.setSelectionRange(start, start + suggest.dataset.text.length);
  }
});

// Suggestions need JavaScript, so their buttons start hidden.
function revealSuggest(root) {
  for (const b of root.querySelectorAll("[data-suggest]")) b.hidden = b.dataset.text === undefined;
}
revealSuggest(document);

// Forms that update as soon as a choice changes (the compare page).
for (const form of document.querySelectorAll("form[data-autosubmit]")) {
  form.addEventListener("change", (event) => {
    // A branch of another repository doesn't exist in the new one.
    const reset = event.target.dataset.resets;
    if (reset) form.elements[reset].disabled = true;
    form.submit();
  });
}

// Reactions: toggle without leaving the spot on the page. The server
// answers with the updated page; only the reactions are swapped in.
document.addEventListener("submit", (event) => {
  const form = event.target.closest(".reaction-form");
  if (!form || !event.submitter) return;
  event.preventDefault();
  const body = new URLSearchParams(new FormData(form, event.submitter));
  form.closest("details.dropdown[open]")?.removeAttribute("open");
  form.querySelector("details[open]")?.removeAttribute("open");
  fetch(form.action, { method: "POST", body })
    .then((r) => (r.ok ? r.text() : Promise.reject()))
    .then((html) => {
      const page = new DOMParser().parseFromString(html, "text/html");
      const fresh = page.getElementById(form.dataset.reactions);
      if (fresh) document.getElementById(form.dataset.reactions)?.replaceWith(fresh);
    })
    .catch(() => {
      // Without the clicked button, a plain submit wouldn't say which reaction.
      const { name, value } = event.submitter;
      form.append(Object.assign(document.createElement("input"), { type: "hidden", name, value }));
      form.submit();
    });
});
