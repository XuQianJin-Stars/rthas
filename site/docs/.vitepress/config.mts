import { defineConfig } from "vitepress";

export default defineConfig({
  title: "rthas",
  description:
    "Arthas-flavoured runtime probe toolkit for Rust: trace, watch, stack, dashboard, profiler, attach --ebpf",
  lang: "en-US",
  base: "/rthas/",
  lastUpdated: true,
  cleanUrls: true,
  ignoreDeadLinks: true,
  themeConfig: {
    logo: false,
    siteTitle: "rthas",
    nav: [
      { text: "Home", link: "/" },
      { text: "Docs", link: "/intro" },
      { text: "Commands", link: "/commands" },
      {
        text: "GitHub",
        link: "https://github.com/XuQianJin-Stars/rthas",
      },
    ],
    sidebar: [
      {
        text: "Docs",
        items: [
          { text: "Introduction", link: "/intro" },
          { text: "Quick Start", link: "/quick-start" },
          { text: "Install", link: "/install" },
        ],
      },
      {
        text: "Commands",
        items: [
          { text: "All Commands", link: "/commands" },
          { text: "trace", link: "/trace" },
          { text: "watch", link: "/watch" },
          { text: "stack", link: "/stack" },
          { text: "stats / top", link: "/stats" },
          { text: "monitor", link: "/monitor" },
          { text: "tt", link: "/tt" },
          { text: "dashboard", link: "/dashboard" },
          { text: "thread", link: "/thread" },
          { text: "profiler", link: "/profiler" },
        ],
      },
      {
        text: "Attach",
        items: [
          { text: "Instrumented process", link: "/attach" },
          { text: "eBPF attach", link: "/ebpf" },
        ],
      },
      {
        text: "Reference",
        items: [
          { text: "Architecture", link: "/architecture" },
          { text: "Environment", link: "/env" },
          { text: "Roadmap", link: "/roadmap" },
        ],
      },
    ],
    socialLinks: [
      { icon: "github", link: "https://github.com/XuQianJin-Stars/rthas" },
    ],
    search: {
      provider: "local",
    },
    outline: {
      level: [2, 3],
      label: "Table of Contents",
    },
    editLink: {
      pattern:
        "https://github.com/XuQianJin-Stars/rthas/edit/main/site/docs/:path",
      text: "Edit this page on GitHub",
    },
    footer: {
      message: "Apache-2.0 license",
      copyright: "Copyright © 2026 rthas contributors",
    },
  },
});
