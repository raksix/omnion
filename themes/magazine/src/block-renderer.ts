/**
 * The Magazine theme's block renderer.
 *
 * The platform owns what a block MEANS (a FAQ is a description list, a testimonial is a
 * figure with a caption), so the switch statement lives in the SDK. What a theme owns is the
 * class prefix its own stylesheet rules match, and the few shapes where presentation is the
 * block's whole point — see `page-layout.tsx` for the Magazine wrappers.
 */
import { createBlockRenderer } from "@omnion/theme-sdk";

/** The block renderer this theme draws content with. */
export const { Block, BlockTree, bodyParagraphs, prefix } = createBlockRenderer({
  prefix: "ma-magazine",
});
