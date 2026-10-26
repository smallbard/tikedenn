// Champs Markdown (description, critères d'acceptation) : onglets
// Preview / Edit, aperçu rendu par le serveur (POST /markdown) et graphes
// mermaid dessinés dans le navigateur. mermaid.min.js (~5 Mo) n'est chargé
// que si la page contient un graphe.
(function () {
  'use strict';

  var mermaidLoad = null;

  function loadMermaid() {
    if (!mermaidLoad) {
      mermaidLoad = new Promise(function (resolve, reject) {
        var s = document.createElement('script');
        s.src = '/static/mermaid.min.js';
        s.onload = function () {
          window.mermaid.initialize({ startOnLoad: false, securityLevel: 'strict', theme: 'dark' });
          resolve(window.mermaid);
        };
        s.onerror = function () {
          mermaidLoad = null;
          reject(new Error('Could not load mermaid'));
        };
        document.head.appendChild(s);
      });
    }
    return mermaidLoad;
  }

  function renderMermaid(root) {
    var nodes = root.querySelectorAll('pre.mermaid:not([data-processed])');
    if (!nodes.length) return;
    loadMermaid()
      .then(function (m) { return m.run({ nodes: nodes, suppressErrors: true }); })
      .catch(function (e) { console.error(e); });
  }

  function show(field, tab) {
    var preview = field.querySelector('.md-preview');
    var area = field.querySelector('textarea');
    field.querySelectorAll('.md-tab').forEach(function (b) {
      b.classList.toggle('active', b.dataset.tab === tab);
    });
    if (tab === 'edit') {
      preview.hidden = true;
      area.hidden = false;
      area.focus();
      return;
    }
    area.hidden = true;
    preview.hidden = false;
    var text = area.value;
    if (field.dataset.rendered === text) return;
    fetch('/markdown', {
      method: 'POST',
      headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
      body: new URLSearchParams({ text: text })
    })
      .then(function (r) { return r.ok ? r.text() : Promise.reject(new Error(r.statusText)); })
      .then(function (html) {
        preview.innerHTML = html;
        field.dataset.rendered = text;
        renderMermaid(preview);
      })
      .catch(function (e) { preview.textContent = 'Preview failed: ' + e.message; });
  }

  document.addEventListener('click', function (e) {
    var tab = e.target.closest('.md-tab');
    if (tab) show(tab.closest('.md-field'), tab.dataset.tab);
  });

  document.addEventListener('DOMContentLoaded', function () {
    // L'aperçu initial correspond au texte enregistré.
    document.querySelectorAll('.md-field').forEach(function (f) {
      f.dataset.rendered = f.querySelector('textarea').value;
    });
    renderMermaid(document);
  });
  document.addEventListener('htmx:afterSwap', function (e) { renderMermaid(e.target); });
})();
