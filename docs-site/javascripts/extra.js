/* Mermaid, if a page still has a pre.mermaid fence, is rendered in Acid Arcade colors. */
(function () {
  function vars(dark) {
    if (dark) {
      return {
        darkMode: true,
        background: "#141518",
        primaryColor: "#1C1D22",
        primaryTextColor: "#E6E8EE",
        primaryBorderColor: "#2E313B",
        secondaryColor: "#262830",
        tertiaryColor: "#141518",
        lineColor: "#00AAFF",
        textColor: "#E6E8EE",
        mainBkg: "#1C1D22",
        nodeBorder: "#2E313B",
        clusterBkg: "#1C1D22",
        clusterBorder: "#2E313B",
        titleColor: "#E6E8EE",
        edgeLabelBackground: "#141518",
        nodeTextColor: "#E6E8EE",
      };
    }
    return {
      darkMode: false,
      background: "#FFFFFF",
      primaryColor: "#F4F5F7",
      primaryTextColor: "#141518",
      primaryBorderColor: "#E2E4EA",
      secondaryColor: "#FFFFFF",
      tertiaryColor: "#FFFFFF",
      lineColor: "#0070B8",
      textColor: "#141518",
      mainBkg: "#F4F5F7",
      nodeBorder: "#E2E4EA",
      clusterBkg: "#F4F5F7",
      clusterBorder: "#E2E4EA",
      titleColor: "#141518",
      edgeLabelBackground: "#FFFFFF",
      nodeTextColor: "#141518",
    };
  }

  async function renderAll() {
    if (!window.mermaid) return;
    var dark = (document.body.getAttribute("data-md-color-scheme") || "slate") === "slate";
    window.mermaid.initialize({
      startOnLoad: false,
      securityLevel: "loose",
      theme: "base",
      themeVariables: vars(dark),
    });
    var nodes = document.querySelectorAll("pre.mermaid");
    for (var i = 0; i < nodes.length; i++) {
      var pre = nodes[i];
      var src = (pre.textContent || "").trim();
      if (!src) continue;
      var id = "nw-mermaid-" + i + "-" + Date.now();
      try {
        var out = await window.mermaid.render(id, src);
        var wrap = document.createElement("div");
        wrap.className = "mermaid";
        wrap.innerHTML = out.svg;
        pre.replaceWith(wrap);
      } catch (e) {
        console.warn("mermaid render failed", e);
      }
    }
  }

  if (typeof document$ !== "undefined") {
    document$.subscribe(function () { renderAll(); });
  } else {
    document.addEventListener("DOMContentLoaded", renderAll);
  }
})();
