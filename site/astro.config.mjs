import starlight from "@astrojs/starlight";
import { defineConfig } from "astro/config";

export default defineConfig({
  integrations: [
    starlight({
      title: "zwinger",
      logo: { src: "./src/assets/cage.svg" },
      favicon: "/favicon.svg",
      customCss: ["./src/styles/theme.css"],
      social: [{ icon: "github", label: "GitHub", href: "https://github.com/driif/zwinger" }],
      sidebar: [
        "get-started",
        "projects",
        "cages",
        "agents",
        "network",
        "tasks",
        "shortcuts",
        "updates",
      ],
    }),
  ],
});
