/*---------------------------------------------------------------------------------------------
 *  Copyright (c) Microsoft Corporation. All rights reserved.
 *  Licensed under the MIT License. See License.txt in the project root for license information.
 *--------------------------------------------------------------------------------------------*/

/**
 * Normalizes whitespace by collapsing runs of whitespace to a single space and trimming the ends.
 */
export function normalizeWhitespace(text: string): string {
	return text.replace(/\s+/g, ' ').trim();
}

const ABBREVIATIONS = new Set([
	'dr.',
	'mr.',
	'mrs.',
	'ms.',
	'prof.',
	'sr.',
	'jr.',
	'vs.',
	'etc.',
	'e.g.',
	'i.e.',
	'u.s.',
	'u.k.',
	'a.m.',
	'p.m.'
]);

/**
 * Splits text into sentences while avoiding false breaks after common abbreviations and inside quoted text.
 */
export function splitIntoSentences(text: string): string[] {
	const normalized = normalizeWhitespace(text);
	if (!normalized) {
		return [];
	}

	const protectedText = normalized.replace(/"[^"]*"|'[^']*'|\b(?:[A-Za-z]\.){2,}|(?:dr|mr|mrs|ms|prof|sr|jr|vs|etc|e\.g|i\.e|u\.s|u\.k|a\.m|p\.m)\./gi, match => {
		return match.replace(/[.?!]/g, '§');
	});

	const sentences = protectedText
		.split(/(?<=[.!?])\s+(?=[A-Z\"'\(])/)
		.map(part => normalizeWhitespace(part))
		.filter(Boolean);

	return sentences.map(sentence => sentence.replace(/§/g, '.'));
}

/**
 * Splits text into sentences and then tokenizes each sentence. Quoted phrases stay together as single tokens.
 */
export function tokenizeText(text: string): string[] {
	const sentences = splitIntoSentences(text);
	const tokens: string[] = [];
	for (const sentence of sentences) {
		const matches = sentence.match(/"[^"]+"|'[^']+'|[A-Za-z0-9]+(?:\.[A-Za-z0-9]+)*\.?|[A-Za-z0-9]+(?:[’'][A-Za-z0-9]+)*(?:-[A-Za-z0-9]+)*/g) ?? [];
		for (const match of matches) {
			const trimmed = match.trim();
			if (trimmed) {
				tokens.push(trimmed);
			}
		}
	}
	return tokens;
}

/**
 * Convenience alias for tokenizing text after sentence splitting.
 */
export const tokenize = tokenizeText;

function isAbbreviationToken(token: string): boolean {
	return ABBREVIATIONS.has(token.toLowerCase());
}

/**
 * Returns the sentence fragments in a text value, treating common abbreviations as part of the sentence.
 */
export function splitTextIntoSentences(text: string): string[] {
	return splitIntoSentences(text);
}
