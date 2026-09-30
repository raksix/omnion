/**
 * The Minimal theme's block renderer.
 *
 * Minimal needs no block overrides, so it is the SDK's renderer with its own class prefix.
 * Having it in its own file is not ceremony: a theme that has to reach past the SDK for a
 * default is a theme that will copy the default the first time it wants to change one thing,
 * and that copy is where ten themes become one theme with ten colour palettes.
 */
import { createBlockRenderer } from "@omnion/theme-sdk";

/** The block renderer this theme draws content with. */
export const { Block, BlockTree, bodyParagraphs, prefix } = createBlockRenderer({
  prefix: "mn",
});
