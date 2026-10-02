import { ThirdPartyEmote } from "../types/ThirdPartyEmote";
import { use7tvChannelEmotes } from "./use7tvChannelEmotes";
import { use7tvGlobalEmotes } from "./use7tvGlobalEmotes";

// 7TV is the only third party emote provider which supports Kick channels.
// BetterTTV and FrankerFaceZ are Twitch only: their numeric room ids belong to Twitch users,
// so they would show the emotes of an unrelated Twitch channel.
export function useThirdPartyEmotes(channelId: string): Array<ThirdPartyEmote> {
	const thirdPartyEmotes: Array<ThirdPartyEmote> = [
		...use7tvChannelEmotes(channelId),
		...use7tvGlobalEmotes(),
	];

	return thirdPartyEmotes;
}
