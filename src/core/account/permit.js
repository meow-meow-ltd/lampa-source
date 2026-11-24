import Storage from '../storage/storage'

let permit = {}

Object.defineProperty(permit, 'provider', {
    get: ()=> (window.lampa_settings.account_provider || 'cub')
})

Object.defineProperty(permit, 'isLocal', {
    get: ()=> permit.provider === 'local'
})

Object.defineProperty(permit, 'isCub', {
    get: ()=> permit.provider === 'cub'
})

// Получить аккаунт
Object.defineProperty(permit, 'account', { 
    get: ()=> {
        let account = Storage.get('account', '{}') || {}

        if(!account.profile) account.profile = {id: 0, icon: 'l_1'}
        if(!account.profile.id) account.profile.id = 0
        if(!account.profile.icon) account.profile.icon = 'l_1'

        return account
    }
})

// Получить профиль аккаунта
Object.defineProperty(permit, 'profile', { 
    get: ()=> permit.account.profile || {}
})

// Получить данные пользователя
Object.defineProperty(permit, 'user', { 
    get: ()=> Storage.get('account_user', '{}')
})

// Получить токен аккаунта
Object.defineProperty(permit, 'token', { 
    get: ()=> permit.account.token
})

// Получить ID профиля (в local режиме токен может отсутствовать)
Object.defineProperty(permit, 'profileId', {
    get: ()=> (permit.account && permit.account.profile ? permit.account.profile.id : 0) || 0
})

// Пользователь залогинен и разрешено использование аккаунта
Object.defineProperty(permit, 'access', { 
    get: ()=> permit.isLocal ? !!window.lampa_settings.account_use : (permit.token && window.lampa_settings.account_use)
})

// Пользователь включил синхронизацию
Object.defineProperty(permit, 'use', { 
    get: ()=>  permit.access && Storage.field('account_use')
})

// Синхронизация включена и разрешено использование синхронизации
Object.defineProperty(permit, 'sync', { 
    get: ()=>  permit.use && window.lampa_settings.account_sync
})

// Детский профиль
Object.defineProperty(permit, 'child', { 
    get: ()=>  permit.access && permit.profile.child
})

// Детский профиль - маленький ребенок (до 14 лет)
Object.defineProperty(permit, 'child_small', { 
    get: ()=>  permit.child ? permit.profile.age <= 14 : false
})

export default permit
