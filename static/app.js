/* Nexus（枢纽）— 前端胶水。
 *
 * 只做三件事，且必须与 src/ 里的约定一一对应，不做"顺手加个功能"：
 *   1. 把 `<meta name="csrf-token">` 的值注入每一个 HTMX 请求头；
 *   2. HTMX 请求失败时给用户一句人话，而不是让按钮无声变灰；
 *   3. 409 / 422 这类"你手慢了/填错了"用不打断输入的提示方式。
 *
 * 没有 import、没有构建步骤、没有框架——Caddy 原样吐出这个文件。
 */

(function () {
  "use strict";

  /** 服务端渲染进 layout.html 的 token；M0 阶段恒为空串。 */
  var meta = document.querySelector('meta[name="csrf-token"]');
  var csrfToken = meta ? meta.getAttribute("content") || "" : "";

  // 动手前先确认 htmx 真的在。少了它页面必须还能用纯 HTML 表单提交（渐进增强），
  // 而不是所有按钮一起死掉。
  var hasHtmx = typeof window.htmx !== "undefined";

  if (hasHtmx) {
    // 每个 HTMX 请求都带 CSRF 头。token 为空时也照常注册，但服务端 M0 不校验；
    // 等 M1 有了登录态，token 自然非空，这段代码不需要再改。
    document.body.addEventListener("htmx:configRequest", function (event) {
      if (csrfToken) {
        event.detail.headers["X-CSRF-Token"] = csrfToken;
      }
    });

    // 服务器返回 5xx：不要让用户盯着一个没变化的按钮猜发生了什么。
    document.body.addEventListener("htmx:responseError", function (event) {
      var status = event.detail.xhr.status;
      var message = "请求没成功（HTTP " + status + "）。刷新页面再试一次。";
      if (status === 413) {
        message = "提交的内容太大了，请精简后再试。";
      } else if (status === 429) {
        message = "操作太频繁了，稍等一会儿再试。";
      }
      showFlash(message, "error");
    });

    // 网络层失败（断网、服务重启）：HTMX 默认什么都不说。
    document.body.addEventListener("htmx:sendError", function () {
      showFlash("连不上服务器，检查网络后重试。", "error");
    });
  }

  /**
   * 在页头 flash 区追加一条消息。复用服务端渲染的同一种 DOM 结构，
   * 样式由 app.css 的 .flash / .flash--* 提供，前端不另写一套。
   */
  function showFlash(text, kind) {
    var host = document.querySelector("[data-flash-host]");
    if (!host) {
      return;
    }
    var el = document.createElement("div");
    el.className = "flash flash--" + (kind || "info");
    el.setAttribute("role", "status");
    el.textContent = text; // textContent：消息里带 <script> 也只会被当字面量
    host.appendChild(el);
  }
})();
