export interface UserLogResponse {
    messages: BasicLogMessage[],
}

export interface LogMessage extends Omit<BasicLogMessage, "timestamp"> {
    timestamp: Date,
    emotes: Array<Emote>,
}

export interface BasicLogMessage {
    text: string,
    displayName: string,
    timestamp: string,
    id: string,
    tags: Record<string, string>,
}

export interface Emote {
    startIndex: number,
    endIndex: number,
    code: string,
    id: string,
}