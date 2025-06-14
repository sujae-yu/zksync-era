import { z } from 'zod';
import { Address } from 'viem';
import { Authorizer } from '@/permissions/authorizer';
import { FastifyReplyType } from 'fastify/types/type-provider';
import { errorResponse, invalidRequest, unauthorized } from './json-rpc';

const rpcReqSchema = z.object({
    id: z.union([z.number(), z.string()]),
    jsonrpc: z.literal('2.0'),
    method: z.string(),
    params: z.array(z.any()).optional()
});

export type RequestContext = {
    authorizer: Authorizer;
    targetRpcUrl: string;
    currentUser: Address;
};

export interface MethodHandler {
    name: string;

    handle(context: RequestContext, method: string, params: unknown[], id: number | string): Promise<FastifyReplyType>;
}

function hasProperty<T, P extends string>(obj: T, prop: P): obj is T & Record<P, unknown> {
    return typeof obj === 'object' && obj !== null && prop in obj;
}

function getErrorCode(err: unknown) {
    if (hasProperty(err, 'code') && typeof err.code === 'number') {
        return err.code;
    }
    return -32603;
}

function getErrorMessage(err: unknown) {
    if (hasProperty(err, 'message') && typeof err.message === 'string') {
        return err.message;
    }
    return '';
}

function getErrorData(err: unknown) {
    if (hasProperty(err, 'data')) {
        const stringifiedData = JSON.stringify(err.data);
        if (stringifiedData !== undefined) {
            err.data = JSON.parse(stringifiedData);
        }
        return err.data;
    }
}

export class RpcCallHandler {
    private handlers: Record<string, MethodHandler>;
    private context: RequestContext;

    constructor(handlers: MethodHandler[], context: RequestContext) {
        this.context = context;
        this.handlers = handlers.reduce<Record<string, MethodHandler>>((acum, current) => {
            acum[current.name] = current;
            return acum;
        }, {});
    }

    async handle(rawBody: unknown): Promise<FastifyReplyType> {
        // Batch request: array of requests
        if (Array.isArray(rawBody)) {
            if (rawBody.length === 0) {
                return invalidRequest(null);
            }
            const results = await Promise.all(rawBody.map(async (item) => this.handleSingleRequest(item)));
            return results;
        }

        // Single request
        return this.handleSingleRequest(rawBody);
    }

    private async handleSingleRequest(request: unknown): Promise<FastifyReplyType> {
        const parsed = rpcReqSchema.safeParse(request);
        if (parsed.error) {
            console.warn('Invalid request!', parsed.error);
            return invalidRequest(null);
        }

        try {
            const { method, params, id } = parsed.data;
            return await this.tryCall(method, params, id);
        } catch (e) {
            console.warn('Error in handler', e);
            return errorResponse({
                id: parsed.data.id,
                error: {
                    code: getErrorCode(e),
                    message: getErrorMessage(e),
                    data: getErrorData(e)
                }
            });
        }
    }

    private async tryCall(method: string, params: unknown[] = [], id: number | string) {
        const handler = this.handlers[method] || this.defaultHandler();
        return handler.handle(this.context, method, params, id);
    }

    private defaultHandler(): MethodHandler {
        return {
            name: 'default-handler',
            handle: (_context, _method, _params, id) => unauthorized(id)
        };
    }
}
