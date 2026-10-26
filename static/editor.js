/* Nexus 富文本编辑器（无依赖，约 200 行）。
 *
 * 设计原则：
 * 1. **渐进增强**：HTML 里先渲染普通 textarea，脚本跑起来才把它藏起来换成 contenteditable。
 *    脚本挂了/被 CSP 拦了，用户退化成"只能打字、不能插图"，而不是对着空白框发呆。
 * 2. **提交时回填**：编辑器把 innerHTML 写进 hidden 字段 body_html；服务端拿不到就回退用纯文本
 *    textarea（名字还是 body）。两条路都能提交，服务端只认优先级。
 * 3. **粘贴清洗交给服务端**：这里只负责把"粘贴的多段 HTML"压成纯文本再插入，
 *    真正的白名单净化在 shared::richtext（前端净化不可信）。
 * 4. **图片走 XHR**：上传成功后把 <img> 插到光标处，不刷新页面（刷新会丢掉已敲的字）。
 */
(function () {
  "use strict";

  function csrfToken() {
    var meta = document.querySelector('meta[name="csrf-token"]');
    return meta ? meta.getAttribute("content") || "" : "";
  }

  function uploadUrl(input) {
    var form = input.closest("form");
    return (form && form.getAttribute("data-upload-url")) || "/media/upload";
  }

  function make(tag, cls) {
    var el = document.createElement(tag);
    if (cls) el.className = cls;
    return el;
  }

  // 上传失败说人话。
  //
  // 服务器返回的是内置错误页（一整页 HTML），直接塞进 alert 是一堵墙；而最常见的那种失败
  // 根本没有响应——nginx 收不下请求体时会直接断连接，浏览器只给一个
  // "TypeError: Failed to fetch"。用户看到的就是"连接失败"，但真正的原因是文件超过了
  // 网关上限。所以这里按状态码和大类翻译：
  function describeFailure(status, text) {
    if (status === 413) {
      return "文件太大，被网关挡下了。图片上限 4 MB、Word 文档上限 8 MB。";
    }
    if (status === 401 || status === 403) {
      return "登录状态过期了，刷新页面重新登录再试。";
    }
    if (status === 422) {
      return "这个文件读不了：请确认它是真正的 Word 文档或图片，且没有损坏。";
    }
    if (status === 429) {
      return "上传太频繁了，等几分钟再试。";
    }
    if (status >= 500) {
      return "服务端处理失败（" + status + "），这不是你的操作问题，稍后再试或告诉管理员。";
    }
    // 其它 4xx：服务端给了具体原因（纯文本），取第一段有效信息，别把整页 HTML 丢给用户。
    var plain = (text || "").replace(/<[^>]*>/g, " ").replace(/\s+/g, " ").trim();
    if (plain.length > 120) plain = plain.slice(0, 120) + "…";
    return plain || ("上传失败（" + status + "）");
  }

  function describeNetwork(err) {
    if (err && err.name === "TypeError") {
      return "连接中断，服务端没收到完整文件。多半是文件接近或超过上限（图片 4 MB、Word 8 MB），也可能是网络不稳，换小一点的文件或重试一次。";
    }
    return (err && err.message) || "未知错误";
  }

  function setup(root) {
    var surface = root.querySelector("[data-surface]");
    var hidden = root.querySelector("[data-html]");
    var fallback = root.querySelector("[data-fallback]");
    var fileInput = root.querySelector("[data-file]");
    var docxInput = root.querySelector("[data-docx-file]");
    var form = root.closest("form");
    if (!surface || !hidden || !fallback || !form) return;

    // 用 textarea 里已有的内容初始化编辑器：按空行分段，文本一律当文本插入（不解析 HTML）。
    var seeded = fallback.value.trim();
    if (seeded) {
      seeded.split(/\n{2,}/).forEach(function (block) {
        var p = make("p");
        p.textContent = block.trim();
        if (p.textContent) surface.appendChild(p);
      });
    }
    if (!surface.textContent.trim()) {
      surface.appendChild(make("p"));
    }

    fallback.hidden = true;
    root.classList.add("editor--ready");

    function sync() {
      hidden.value = surface.innerHTML;
    }

    function focusSurface() {
      surface.focus();
    }

    function exec(cmd, value) {
      focusSurface();
      try {
        document.execCommand(cmd, false, value || null);
      } catch (err) {
        /* execCommand 在个别浏览器对未知命令会抛，忽略即可 */
      }
      sync();
    }

    function insertNode(node) {
      focusSurface();
      var selection = window.getSelection();
      if (selection && selection.rangeCount) {
        var range = selection.getRangeAt(0);
        range.deleteContents();
        range.insertNode(node);
        range.setStartAfter(node);
        range.collapse(true);
        selection.removeAllRanges();
        selection.addRange(range);
      } else {
        surface.appendChild(node);
      }
      sync();
    }

    root.addEventListener("click", function (event) {
      var button = event.target.closest("[data-cmd]");
      if (button) {
        event.preventDefault();
        var cmd = button.getAttribute("data-cmd");
        if (cmd === "createLink") {
          var href = window.prompt("链接地址（https:// 开头，或以 / 开头的站内路径）");
          if (href && href.trim()) exec("createLink", href.trim());
          return;
        }
        if (cmd === "formatBlock") {
          exec("formatBlock", "<" + button.getAttribute("data-value") + ">");
          return;
        }
        exec(cmd);
        return;
      }

      if (event.target.closest("[data-upload]")) {
        event.preventDefault();
        if (fileInput) fileInput.click();
        return;
      }

      if (event.target.closest("[data-docx]")) {
        event.preventDefault();
        if (docxInput) docxInput.click();
      }
    });

    if (fileInput) {
      fileInput.addEventListener("change", function () {
        var file = fileInput.files && fileInput.files[0];
        fileInput.value = "";
        if (!file) return;
        if (file.size > 4 * 1024 * 1024) {
          window.alert("图片不能超过 4 MB，请先压缩。");
          return;
        }
        var body = new FormData();
        body.append("file", file);
        var token = csrfToken();
        fetch(uploadUrl(root), {
          method: "POST",
          body: body,
          credentials: "same-origin",
          headers: token ? { "X-CSRF-Token": token } : {},
        })
          .then(function (response) {
            if (!response.ok) {
              return response.text().then(function (text) {
                throw new Error(describeFailure(response.status, text));
              });
            }
            return response.json();
          })
          .then(function (data) {
            if (!data || !data.url) throw new Error("上传没有返回地址");
            var img = make("img");
            img.src = data.url;
            img.alt = file.name || "图片";
            insertNode(img);
          })
          .catch(function (err) {
            window.alert("图片上传失败：" + describeNetwork(err));
          });
      });
    }

    // 插入 Word 文档：服务端把 .docx 解析成 HTML 存进媒体库，这里只放一个受限的 iframe。
    // 编辑区里存的是 <figure class="doc-embed"><iframe src="/doc/<sha>">…，不是正文的副本——
    // 文档改了要重新上传，换来的是"编辑器里的 HTML 永远比正文小一个量级"。
    function insertDocEmbed(url, title) {
      var figure = make("figure", "doc-embed");
      var frame = make("iframe");
      frame.setAttribute("src", url);
      frame.setAttribute("sandbox", "");
      frame.setAttribute("loading", "lazy");
      frame.setAttribute("title", title || "Word 文档");
      figure.appendChild(frame);
      if (title) {
        var caption = make("figcaption");
        caption.textContent = title;
        figure.appendChild(caption);
      }
      insertNode(figure);
    }

    if (docxInput) {
      docxInput.addEventListener("change", function () {
        var file = docxInput.files && docxInput.files[0];
        docxInput.value = "";
        if (!file) return;
        if (file.size > 8 * 1024 * 1024) {
          window.alert("Word 文档不能超过 8 MB。");
          return;
        }
        var body = new FormData();
        body.append("file", file);
        var token = csrfToken();
        fetch(root.getAttribute("data-docx-url") || "/media/docx", {
          method: "POST",
          body: body,
          credentials: "same-origin",
          headers: token ? { "X-CSRF-Token": token } : {},
        })
          .then(function (response) {
            if (!response.ok) {
              return response.text().then(function (text) {
                throw new Error(describeFailure(response.status, text));
              });
            }
            return response.json();
          })
          .then(function (data) {
            if (!data || !data.url) throw new Error("上传没有返回地址");
            insertDocEmbed(data.url, data.title);
          })
          .catch(function (err) {
            window.alert("Word 上传失败：" + describeNetwork(err));
          });
      });
    }

    // 从 Word / 网页复制过来的 HTML：先按白名单重建成结构再插入。
    //
    // 为什么不能直接 insertHTML：Word 会带进 mso-* 样式、<o:p>、整页 <style> 与字体声明，
    // 粘一次后面板就变成"网页中毒"；而一律当纯文本又丢掉了加粗、列表、表格——
    // 后者正是 owner 这次要的"复制保留格式"。所以走中间那条：结构留下，样式全扔。
    //
    // 注意这只是体验层的清洗，安全边界仍在服务端 richtext::sanitize（它不信任这里的输出）。
    var KEEP = {
      p: 1, br: 1, strong: 1, b: 1, em: 1, i: 1, u: 1, s: 1, del: 1,
      ul: 1, ol: 1, li: 1, h2: 1, h3: 1, blockquote: 1, pre: 1, code: 1,
      a: 1, hr: 1, div: 1, span: 1, table: 1, thead: 1, tbody: 1, tfoot: 1,
      tr: 1, th: 1, td: 1, img: 1,
    };
    var DROP = {
      script: 1, style: 1, head: 1, title: 1, meta: 1, link: 1, iframe: 1, object: 1,
      embed: 1, svg: 1, math: 1, form: 1, input: 1, textarea: 1, button: 1,
      select: 1, option: 1, template: 1, noscript: 1,
    };

    function escapeText(value) {
      return String(value)
        .replace(/&/g, "&amp;")
        .replace(/</g, "&lt;")
        .replace(/>/g, "&gt;");
    }

    function escapeAttr(value) {
      return escapeText(value).replace(/"/g, "&quot;");
    }

    function safeHref(href) {
      if (!href) return false;
      if (/^\/(?!\/)/.test(href)) return true; // 站内单斜杠
      if (/^https?:\/\//i.test(href)) return true;
      if (/^mailto:/i.test(href)) return true;
      return false;
    }

    function safeImageSrc(src) {
      if (!src) return false;
      if (/^\/uploads\//.test(src)) return true;
      return /^https?:\/\//i.test(src);
    }

    function spanValue(node, name) {
      var raw = node.getAttribute(name);
      if (!raw) return "";
      var value = parseInt(raw, 10);
      if (!value || value < 2 || value > 64) return "";
      return ' ' + name + '="' + value + '"';
    }

    function cleanHtml(html) {
      var doc;
      try {
        doc = new DOMParser().parseFromString(html, "text/html");
      } catch (err) {
        return "";
      }
      if (!doc || !doc.body) return "";

      function walk(node, out) {
        var kids = node.childNodes;
        for (var i = 0; i < kids.length; i += 1) {
          var child = kids[i];
          if (child.nodeType === 3) {
            out.push(escapeText(child.nodeValue));
            continue;
          }
          if (child.nodeType !== 1) continue;
          var name = child.tagName.toLowerCase();
          if (name.indexOf(":") >= 0) {
            walk(child, out); // <o:p> / <v:imagedata> 之类：拆掉壳，内容照走
            continue;
          }
          if (DROP[name]) continue;
          if (!KEEP[name]) {
            walk(child, out);
            continue;
          }
          var attrs = "";
          if (name === "a") {
            var href = (child.getAttribute("href") || "").trim();
            if (!safeHref(href)) {
              walk(child, out);
              continue;
            }
            attrs = ' href="' + escapeAttr(href) + '"';
          }
          if (name === "img") {
            var src = (child.getAttribute("src") || "").trim();
            if (!safeImageSrc(src)) continue;
            out.push('<img src="' + escapeAttr(src) + '" alt="' + escapeAttr(child.getAttribute("alt") || "") + '">');
            continue;
          }
          if (name === "td" || name === "th") {
            attrs = spanValue(child, "colspan") + spanValue(child, "rowspan");
          }
          out.push("<" + name + attrs + ">");
          if (name === "br" || name === "hr") continue;
          walk(child, out);
          out.push("</" + name + ">");
        }
      }

      var out = [];
      walk(doc.body, out);
      return out.join("");
    }

    // 粘贴：带图片文件的走上传；带 HTML 的走白名单清洗；否则纯文本。
    surface.addEventListener("paste", function (event) {
      var clipboard = event.clipboardData;
      var items = clipboard && clipboard.items;
      if (items) {
        for (var i = 0; i < items.length; i += 1) {
          if (items[i].type && items[i].type.indexOf("image/") === 0) {
            var blob = items[i].getAsFile();
            if (blob && fileInput) {
              var transfer = new DataTransfer();
              transfer.items.add(blob);
              fileInput.files = transfer.files;
              fileInput.dispatchEvent(new Event("change"));
              event.preventDefault();
              return;
            }
          }
        }
      }

      var html = clipboard ? clipboard.getData("text/html") : "";
      if (html && html.length <= 200000) {
        var cleaned = cleanHtml(html);
        if (cleaned.trim()) {
          event.preventDefault();
          exec("insertHTML", cleaned);
          return;
        }
      }

      var text = clipboard ? clipboard.getData("text/plain") : "";
      if (text) {
        event.preventDefault();
        exec("insertText", text);
      }
    });

    form.addEventListener("submit", sync);
    surface.addEventListener("input", sync);
    surface.addEventListener("blur", sync);
  }

  function boot() {
    document.querySelectorAll("[data-editor]").forEach(setup);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", boot);
  } else {
    boot();
  }
})();
