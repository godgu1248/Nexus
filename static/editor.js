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

  function setup(root) {
    var surface = root.querySelector("[data-surface]");
    var hidden = root.querySelector("[data-html]");
    var fallback = root.querySelector("[data-fallback]");
    var fileInput = root.querySelector("[data-file]");
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
                throw new Error(text || "上传失败（" + response.status + "）");
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
            window.alert("图片上传失败：" + err.message);
          });
      });
    }

    // 粘贴：带图片文件的走上传；其余一律当纯文本，避免把外部页面的 style/字体一起粘进来。
    surface.addEventListener("paste", function (event) {
      var items = event.clipboardData && event.clipboardData.items;
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
      var text = event.clipboardData ? event.clipboardData.getData("text/plain") : "";
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
